//! Bundle importer (F1d): unpack a `.chairphoto` bundle, copy originals + sidecars
//! into the local library, index them via the UUID-aware identity upsert, run the
//! F1c additive merge, and auto-enqueue backup for every new photo.
//!
//! ## Phases
//!
//! 1. **Parse** — open the zip, read `manifest.json`, validate `format_version`.
//! 2. **Copy** (off the catalog lock, slow) — extract each `originals/<relative_path>`
//!    into `<root>/YYYY/MM/DD/<filename>`, using card ingest's collision rules (#246):
//!    - A same-name, same-size file there that is the same capture (EXIF capture time,
//!      sub-second, camera serial; the contents when neither has a capture time) →
//!      already imported, skip.
//!    - Any other collision → rename with ` (n)` suffix (never overwrite).
//!    A collision is decided against the original's bytes in memory, never by unpacking
//!    them beside the library file first.
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
use crate::scanner::Indexed;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Summary returned by `import_bundle_cmd` after a successful import.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleImportResult {
    /// Photos copied from the bundle originals (new to the filesystem).
    pub copied: usize,
    /// Originals skipped because the library already held them at their destination (the
    /// same name, size and capture, #246).
    pub skipped_duplicate: usize,
    /// Originals that encountered a non-fatal error during extraction (metadata-only).
    pub errors: usize,
    /// Originals unpacked back onto the row of their identity that had lost its file — the
    /// row re-linked, not a new one (#247) — and how many of those rows are in the trash.
    pub restored: usize,
    pub restored_trashed: usize,
    /// Originals whose photo was offloaded — its row has no local location left and a
    /// verified backup (#231 F5): already imported, not copied back.
    pub offloaded: usize,
    /// Originals whose name is too long for a sidecar beside it (`<name>.xmp` over 255
    /// bytes): not unpacked, since the photo's identity could never be written.
    pub name_too_long: usize,
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
    /// `dest` was in the library before this import: the same capture, found there and not
    /// copied (#246). Its sidecar is the owner's, so the index phase binds an identity to it
    /// only when no row of another identity holds the file ([`index_bundle`]).
    pub already_in_library: bool,
    /// For a file found already in the library: its capture stamps and the bundle original's
    /// prove the same capture by the strict re-link rule
    /// ([`same_capture_without_contents`](crate::scanner::same_photo::same_capture_without_contents)),
    /// so a row of another identity holding it is this photo's row (#249).
    pub capture_proven: bool,
}

/// Extract originals from `archive` into `dest_base` under a `YYYY/MM/DD` tree,
/// mirroring the `ORIGINALS_DIR/<relative_path>` archive entries. Sidecars
/// (`<entry>.xmp`) are extracted beside their original.
///
/// Collision rules (card ingest's, #246 — `scanner::same_photo`):
/// - A file of the same name and size at the destination (or at one of the ` (n)` names an
///   earlier import gave a different photo of that name) that is the same capture — EXIF
///   capture time, sub-second and camera serial agree, or, with no capture time on either
///   side, the contents — is this photo, already imported → `skipped_duplicate` (no write).
/// - Any other collision → ` (n)` suffix (never overwrite).
/// - A name `catalog`'s rows hold is not free, even with its file gone (#247), unless the
///   row has this photo's identity: the original then goes back to that name and the index
///   phase re-links the row ([`free_name`](crate::scanner::free_name)). `catalog` is the
///   catalog the import indexes into, read on the import's own connection.
///
/// `on_progress(done, total)` is called once per original (including skipped/error)
/// so the caller can stream `import:progress` events.
pub fn extract_originals(
    catalog: &Catalog,
    manifest: &BundleManifest,
    archive: &mut ZipArchive<std::fs::File>,
    dest_base: &Path,
    on_progress: impl Fn(usize, usize),
) -> Result<(Vec<ExtractedItem>, BundleImportResult), String> {
    let never = std::sync::atomic::AtomicBool::new(false);
    extract_originals_abortable(catalog, manifest, archive, dest_base, &never, on_progress)
        .map(|(extracted, result, _)| (extracted, result))
}

/// [`extract_originals`], stopping before the next original once `abort` is set (a Cancel,
/// a newer import or a catalog switch). The third value is whether it stopped early.
///
/// A stop never leaves a half-written file: `abort` is read between originals, and each
/// original is written whole. What was unpacked before the stop **stays** in the library
/// folder, each copy with its identity sidecar, and nothing is deleted. An "already here"
/// entry may be the user's own pre-existing original, and this function will not decide
/// which files it may remove. Importing the bundle again finishes the job: the copies are
/// then the same captures at their names, skipped, bound by UUID and indexed. A rescan also
/// picks them up under the bundle's identity.
///
/// A collision is decided as each original comes out of the bundle, against its bytes in
/// memory ([`same_photo::find_in_library`](crate::scanner::same_photo::find_in_library)):
/// the manifest carries no capture time or serial, and the bytes are never written anywhere
/// to be compared, so re-importing a bundle the library already holds writes nothing to the
/// library's disk.
pub fn extract_originals_abortable(
    catalog: &Catalog,
    manifest: &BundleManifest,
    archive: &mut ZipArchive<std::fs::File>,
    dest_base: &Path,
    abort: &std::sync::atomic::AtomicBool,
    on_progress: impl Fn(usize, usize),
) -> Result<(Vec<ExtractedItem>, BundleImportResult, bool), String> {
    use crate::scanner::same_photo;

    let total = manifest.photos.len();
    let mut result = BundleImportResult {
        copied: 0,
        skipped_duplicate: 0,
        errors: 0,
        restored: 0,
        restored_trashed: 0,
        offloaded: 0,
        name_too_long: 0,
        merge: MergeSummary::default(),
    };
    let mut extracted: Vec<ExtractedItem> = Vec::new();
    // Each date folder is listed once for the unpack, and told of every name placed in it.
    let mut listings = same_photo::FolderListings::default();
    // The names the catalog's rows hold, read once per date folder (#247).
    let mut names = crate::scanner::free_name::CatalogNames::new(catalog);

    for (i, bp) in manifest.photos.iter().enumerate() {
        if abort.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok((extracted, result, true));
        }
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

        let dest = dir.join(filename);
        if !same_photo::sidecar_name_fits(&dest) {
            eprintln!(
                "bundle import: {} not unpacked: its name is too long for a sidecar beside it",
                bp.relative_path
            );
            result.name_too_long += 1;
            continue;
        }

        // A same-name, same-size file may be this photo, already imported (#246): decided
        // now, against the bytes in memory.
        let candidates = listings.same_size_candidates(&dest, orig_bytes.len() as u64);
        let Some(mut already) = same_photo::find_in_library_proving(&orig_bytes, &candidates, abort) else {
            return Ok((extracted, result, true));
        };
        if already.is_none() {
            // About to copy: the folder's listing may be older than this original (it is held
            // for the unpack), so a ` (n)` written there since — by another program or import —
            // is looked at now, and the same photo there is not copied once more (N-3 of the
            // third #246 review). One listing per copied original, never per skipped one.
            listings.relist(&dir);
            let newer: Vec<PathBuf> = listings
                .same_size_candidates(&dest, orig_bytes.len() as u64)
                .into_iter()
                .filter(|c| !candidates.contains(c))
                .collect();
            let Some(found) = same_photo::find_in_library_proving(&orig_bytes, &newer, abort) else {
                return Ok((extracted, result, true));
            };
            already = found;
        }
        if let Some(existing) = already {
            // The same capture is already in the library: skip the copy. Nothing is written
            // beside it here — not even the bundle's identity into its sidecar: the index
            // phase decides that, knowing which row holds the file.
            result.skipped_duplicate += 1;
            extracted.push(ExtractedItem {
                dest: existing.path,
                photo_uuid: bp.uuid.clone(),
                relative_path: bp.relative_path.clone(),
                already_in_library: true,
                capture_proven: existing.proven,
            });
            continue;
        }

        // Any other collision — with a file, or with a catalog row whose file is gone — is a
        // different photo → ` (n)`, claimed so that a file placed there meanwhile is never
        // overwritten. The name of a row with this photo's identity is its own: the original
        // goes back there, and indexing re-links the row (#247).
        let arriving = crate::scanner::free_name::Arriving::Identity(crate::catalog::photo_identity_for(&bp.uuid));
        // The row of this identity was offloaded (a verified backup holds its photo): already
        // imported, not copied back to this disk (#231 F5). The merge still finds the row by
        // identity, as for an original the bundle does not carry.
        if names.kept_elsewhere(&dest, &arriving).is_some() {
            result.offloaded += 1;
            continue;
        }
        let placed = same_photo::create_new_file(&dest, &mut names, &arriving, |file| {
            use std::io::Write;
            file.write_all(&orig_bytes)
        });
        let dest = match placed {
            Ok(dest) => dest,
            Err(e) => {
                eprintln!("bundle import: placing {} ({}) failed: {e}", bp.uuid, dest.display());
                result.errors += 1;
                continue;
            }
        };
        listings.placed(&dest);
        result.copied += 1;
        place_sidecar(archive, &arc_orig, &dest, bp);
        extracted.push(ExtractedItem {
            dest,
            photo_uuid: bp.uuid.clone(),
            relative_path: bp.relative_path.clone(),
            already_in_library: false,
            capture_proven: false,
        });
    }

    Ok((extracted, result, false))
}

/// The sidecar of an original just placed at `dest` — the UUID must land in it so that
/// index_bundle's `upsert_photo_with_identity` (and future re-scans) can match this photo by
/// UUID instead of minting a duplicate.
///
/// 1. If the bundle carries a sidecar for this original, extract it as-is (it already
///    contains `xmp:Identifier`) — as a new file only: an existing file at the sidecar's name
///    is never replaced (`dest` was chosen with that name free, so one there now is another
///    program's, and is left as it is — or, where `dest` re-links the row of this photo's
///    identity (#247), the row's own sidecar, already carrying that identity).
/// 2. Otherwise, write a fresh merge-safe UUID sidecar from the bundle UUID now, before the
///    index phase runs — this is the binding invariant (AGENTS.md). A failure here is not
///    the end of it — the photo has no catalog row yet, so index_bundle is the one that
///    records the repair once the row exists. A blank manifest uuid is no identity (#146
///    N4); the indexer mints one and binds it.
fn place_sidecar(
    archive: &mut ZipArchive<std::fs::File>,
    arc_orig: &str,
    dest: &Path,
    bp: &crate::bundle::BundlePhoto,
) {
    let sidecar_dest = {
        let mut s = dest.as_os_str().to_os_string();
        s.push(".xmp");
        PathBuf::from(s)
    };
    let arc_sidecar = format!("{}.xmp", arc_orig);
    // Never over an existing file: `dest` was chosen with its sidecar's name free
    // (`free_name::CatalogNames::destination`), so one there now appeared meanwhile and is
    // someone else's — or is the sidecar of the row `dest` re-links, of this identity.
    let write_new = |bytes: &[u8]| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&sidecar_dest)?;
        file.write_all(bytes).and_then(|()| file.sync_all()).inspect_err(|_| {
            let _ = std::fs::remove_file(&sidecar_dest);
        })
    };
    let bundle_sidecar_extracted = if let Ok(mut entry) = archive.by_name(&arc_sidecar) {
        let mut sidecar_bytes = Vec::new();
        if entry.read_to_end(&mut sidecar_bytes).is_ok() {
            if let Err(e) = write_new(&sidecar_bytes) {
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
    let identity = crate::catalog::photo_identity_for(&bp.uuid);
    if let (false, false, Some(identity)) = (bundle_sidecar_extracted, sidecar_dest.exists(), identity) {
        if let Err(e) = crate::xmp::write_identifier(dest, &identity) {
            eprintln!(
                "bundle import: couldn't write UUID sidecar for {} — queued for repair: {e}",
                dest.display()
            );
        }
    }
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
/// already-present photo (same UUID) is merely updated in place. For a row created for
/// the bundle's own photo the bundle's rating/label/pick/IPTC/edit-record/versions are
/// applied now (the same pattern as E5's `index_ingested` + `set_photo_metadata`). A row
/// created under another identity — a copy kept apart from the row holding the bundle's
/// identity (#150) — gets none of it (#185).
///
/// **Step B — F1c merge**: run `merge_bundle_into` for the taxonomy, import batch, and
/// tag assignments, told which rows Step A created (`fresh`). A photo already in the
/// catalog — the file skipped as already imported (#246), or matched by identity — has
/// what it lacks filled in: blank culling, the bundle's edits as new versions (#185).
/// Photos that had no originals in the bundle (metadata-only / offline originals) are
/// inserted by the merge if not already present, or filled in like any existing photo.
///
/// **Step C — Post-index** (file I/O): write the batch UUID sidecar (K3) per file,
/// write a new photo's owed IPTC, apply auto-tags, pair RAW+JPEG stacks, and run
/// `reconcile_missing` (O(n) stat checks). These happen on the secondary connection so the
/// main mutex stays free.
///
/// **Step D — an existing photo's blank IPTC** (#185, between C.1 and the auto-tags): the
/// fields the merge found blank on the row are filled where the photo's sidecar has no
/// value either, through `set_iptc` under the sidecar's write turn ([`fill_blank_iptc`]).
/// An existing photo is neither queued for backup nor put in the bundle's batch: this
/// import did not add it, and its batch is the immutable one it arrived with.
///
/// `dest_base` must be a path under the catalog root (same as E5's requirement).
pub fn index_bundle(
    catalog: &Catalog,
    manifest: &BundleManifest,
    extracted: &[ExtractedItem],
    dest_base: &Path,
    partial_result: BundleImportResult,
) -> Result<BundleImportResult, String> {
    index_bundle_abortable(catalog, manifest, extracted, dest_base, partial_result, &AtomicBool::new(false))
        .map(|i| i.result)
}

/// [`index_bundle`], stopping before the next original once `abort` is set (Cancel, a newer
/// import, a catalog switch). A stop leaves a consistent catalog, as if the bundle had held
/// only the originals indexed so far: Step A is committed for each (its row with the
/// bundle's identity, its identity sidecar or queued repair, the bundle's culling, IPTC,
/// edit and versions for a new photo, its queued backup), and Steps B to D run over the
/// manifest narrowed to those photos — the batch and their tags merged, an existing
/// photo's blanks filled, the batch assigned and written into their sidecars, auto-tags,
/// stacks, reconcile. The narrowing matters: the full merge would insert the originals not
/// yet indexed as metadata-only rows. Importing
/// the bundle again finishes it; the upsert is UUID-aware, so the photos indexed here are
/// matched, not duplicated. [`Indexed`] says how many originals were indexed.
pub fn index_bundle_abortable(
    catalog: &Catalog,
    manifest: &BundleManifest,
    extracted: &[ExtractedItem],
    dest_base: &Path,
    partial_result: BundleImportResult,
    abort: &AtomicBool,
) -> Result<Indexed<BundleImportResult>, String> {
    index_bundle_with(catalog, manifest, extracted, dest_base, partial_result, abort, &|_| {})
}

/// [`index_bundle_abortable`], calling `after_each(indexed)` after each original — where a
/// test trips the abort mid-pass.
pub(crate) fn index_bundle_with(
    catalog: &Catalog,
    manifest: &BundleManifest,
    extracted: &[ExtractedItem],
    dest_base: &Path,
    mut partial_result: BundleImportResult,
    abort: &AtomicBool,
    after_each: &dyn Fn(usize),
) -> Result<Indexed<BundleImportResult>, String> {
    use crate::catalog::PickState;

    let total = extracted.len();
    let mut indexed = 0;
    let folder_id = catalog.add_folder(dest_base).map_err(|e| e.to_string())?;

    // Build a lookup table (uuid, relative path) → BundlePhoto for fast access during the
    // upsert loop. Keyed by both because a pre-#146 bundle can carry several photos with a
    // blank uuid, which the uuid alone would collapse into one (#150).
    let bp_by_key: std::collections::HashMap<(&str, &str), &crate::bundle::BundlePhoto> = manifest
        .photos
        .iter()
        .map(|bp| ((bp.uuid.as_str(), bp.relative_path.as_str()), bp))
        .collect();

    let mut newly_created: Vec<i64> = Vec::new();
    // The rows created for the bundle's own photos, which Step A gives the bundle's state.
    let mut fresh: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut versions_added_to: Vec<i64> = Vec::new();
    let mut upserted_copies: Vec<(i64, PathBuf)> = Vec::new();
    // The identity of the row each blank-uuid original was indexed into, by the manifest's
    // relative path: merge has no identity to match such a photo by, and the photo at its
    // path need not be it (#150), so it is told which row the index phase chose.
    let mut indexed_blank: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    // The identities of the bundle's photos whose original is in the library already as
    // another identity's photo: the merge keeps them apart too, even where their own path is
    // free (the original found at a ` (n)` name), rather than inserting a row there that
    // describes no file of theirs.
    let mut kept_apart: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The bundle's photos found in the library as the same capture under another identity,
    // proven by the strict re-link rule (#249), by (uuid, relative path): the identity of the
    // row holding each, which the merge fills instead of keeping the photo apart.
    let mut matched_by_capture: std::collections::HashMap<(&str, &str), String> = std::collections::HashMap::new();

    let tx = catalog.begin().map_err(|e| e.to_string())?;
    for item in extracted {
        if abort.load(Ordering::Relaxed) {
            break; // Step A is committed for what is indexed so far; see the docs
        }
        indexed += 1;
        let path = &item.dest;

        // A file the library already had (#246) that a row of another identity holds is that
        // row's photo, not the bundle's: the same capture imported separately on each side.
        // It is kept apart — no upsert, no identity bound into the owner's sidecar (which may
        // lack one, as identity debt) — and the merge, told so, counts the bundle's photo kept
        // apart when no row holds its identity (`MergeSummary::photos_kept_apart`), whether or
        // not its own relative path is free.
        if item.already_in_library {
            if let Some(held_by) = held_by_another_identity(catalog, path, &item.photo_uuid)? {
                // #249 (decision 2026-10-06): the strict re-link rule proves the same capture
                // — the card was imported on both machines, each minting its own identity — and
                // no row holds the bundle's identity: the bundle's data goes onto that row,
                // which keeps its own identity (and its sidecar its own `xmp:Identifier`).
                let identity = crate::catalog::photo_identity_for(&item.photo_uuid);
                let identity_unheld = identity.as_deref().is_some_and(|id| {
                    use rusqlite::OptionalExtension;
                    catalog
                        .conn()
                        .query_row("SELECT 1 FROM photos WHERE uuid = ?1", [id], |_| Ok(()))
                        .optional()
                        .is_ok_and(|row| row.is_none())
                });
                if item.capture_proven && identity_unheld {
                    eprintln!(
                        "bundle import: {} is photo {held_by}, the same capture as bundle photo {}; merged onto it",
                        path.display(),
                        item.photo_uuid
                    );
                    matched_by_capture.insert((item.photo_uuid.as_str(), item.relative_path.as_str()), held_by);
                    after_each(indexed);
                    continue;
                }
                eprintln!(
                    "bundle import: {} is already photo {held_by}'s; bundle photo {} kept apart",
                    path.display(),
                    item.photo_uuid
                );
                if let Some(identity) = crate::catalog::photo_identity_for(&item.photo_uuid) {
                    kept_apart.insert(identity);
                }
                after_each(indexed);
                continue;
            }
        }

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
        // actually on disk, which for a name collision (#246) need not be the bundle's
        // photo. When it has none (the copy phase's write failed, or the existing
        // sidecar doesn't parse), fall back to the manifest: this photo's identity is
        // known, so minting a fresh UUID here would be inventing a second one for it
        // and leaving `merge_bundle` to insert the bundle's photo a second time.
        // A sidecar identifier that is not a UUID is another tool's, not an identity (#141).
        // `upsert_photo_with_identity` canonicalises it, and maps a manifest id that is not a
        // UUID (a bundle from a pre-#146 catalog) to the identity v23 gave it (#146).
        let identity = sidecar_uuid
            .as_deref()
            .filter(|v| crate::catalog::is_photo_identity(v))
            .unwrap_or(item.photo_uuid.as_str());

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
                after_each(indexed);
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
        upserted_copies.push((upsert.id, path.clone()));
        if crate::catalog::photo_identity_for(&item.photo_uuid).is_none() {
            indexed_blank.insert(item.relative_path.as_str(), upsert.uuid.clone());
        }

        // A row created here is the bundle photo's own unless it took another identity: a copy
        // kept apart from the row holding the bundle's identity, whose original is still in
        // place (#150). That row, which merge matches by identity, gets the bundle's state
        // (filled in where it lacks it, Step B); the copy gets none of it (#185).
        let bundles_own = crate::catalog::photo_identity_for(&item.photo_uuid)
            .is_none_or(|identity| identity == upsert.uuid);
        if upsert.created {
            newly_created.push(upsert.id);
        } else if !item.already_in_library {
            // An original unpacked onto a row that had lost its file: restored, not added
            // (#247). Counted with whether it is in the trash, so it is not invisible.
            partial_result.restored += 1;
            if catalog.is_trashed(upsert.id).unwrap_or(false) {
                partial_result.restored_trashed += 1;
            }
        }
        if upsert.created && bundles_own {
            fresh.insert(upsert.id);

            // Apply the bundle's non-destructive state for this brand-new photo.
            // The F1c merge won't do this because the upsert already made the photo
            // "existing" (it fills in only what an existing row lacks, and is told this
            // row is fresh). This mirrors E5's set_photo_metadata call after upsert.
            if let Some(bp) = bp_by_key.get(&(item.photo_uuid.as_str(), item.relative_path.as_str())) {
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
                    // Owe only what the sidecar beside the file has no value for: a value
                    // there is the bundle's own sidecar's (perhaps another tool's edit the
                    // source catalog never imported), and the import must not overwrite it
                    // (#144; review of #148, M1). A sidecar that does not parse tells us
                    // nothing, so everything is owed (and stays owed until it parses).
                    let carried =
                        crate::xmp::read_iptc_present(path).unwrap_or(crate::catalog::IptcMask::NONE);
                    let _ = catalog.set_iptc_carried(upsert.id, iptc, carried);
                }

                // Edit record (opaque JSON for the editing module).
                if let Some(edit_json) = &bp.edit_record {
                    let trimmed = edit_json.trim();
                    if !trimmed.is_empty() {
                        let _ = catalog.set_edit_record(upsert.id, trimmed);
                    }
                }

                // Named versions, in bundle order.
                if !bp.versions.is_empty() {
                    versions_added_to.push(upsert.id);
                }
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
        after_each(indexed);
    }

    // Auto-enqueue backup for newly-created photos (E4). Failures are best-effort.
    for &photo_id in &newly_created {
        let _ = catalog.enqueue_operation("backup", photo_id);
    }

    tx.commit().map_err(|e| e.to_string())?;

    // Stopped early: Steps B and C cover only the photos indexed (see the docs).
    // A blank-uuid photo whose original was indexed carries, for the merge, the identity of
    // the row it was indexed into (#150).
    let prepared;
    let manifest = if indexed < total || !indexed_blank.is_empty() || !matched_by_capture.is_empty() {
        let done: std::collections::HashSet<(&str, &str)> = extracted[..indexed]
            .iter()
            .map(|i| (i.photo_uuid.as_str(), i.relative_path.as_str()))
            .collect();
        prepared = BundleManifest {
            photos: manifest
                .photos
                .iter()
                .filter(|p| indexed == total || done.contains(&(p.uuid.as_str(), p.relative_path.as_str())))
                .map(|p| {
                    let mut p = p.clone();
                    if let Some(row) = matched_by_capture.get(&(p.uuid.as_str(), p.relative_path.as_str())) {
                        // Merged onto the row of the same capture, which keeps its identity.
                        p.uuid = row.clone();
                    } else if crate::catalog::photo_identity_for(&p.uuid).is_none() {
                        if let Some(identity) = indexed_blank.get(p.relative_path.as_str()) {
                            p.uuid = identity.clone();
                        }
                    }
                    p
                })
                .collect(),
            ..manifest.clone()
        };
        &prepared
    } else {
        manifest
    };

    // Step B — F1c merge: apply taxonomy, import batch, tag assignments — additive.
    // Photos with originals in the bundle are already "existing" after the upsert above;
    // the merge inserts metadata-only photos (no original in bundle) if not present, and
    // unions tag assignments for all. An existing photo (one this import did not create,
    // so not `fresh`) has what it lacks filled in from the bundle: culling, new versions,
    // and — Step D — IPTC (#185).
    let merged = catalog
        .merge_bundle_into(manifest, &fresh, &kept_apart)
        .map_err(|e| e.to_string())?;
    let merge_summary = merged.summary;
    versions_added_to.extend(merged.versions_added_to);

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

    // Step C — K3: write each photo's immutable batch UUID into its sidecar so the
    // batch survives catalog loss/merge across machines. Failures are queued as
    // retryable sidecar debt.
    for (photo_id, path) in &upserted_copies {
        if let Ok(Some(batch_uuid)) = catalog.import_batch_uuid_for_photo(*photo_id) {
            if let Err(e) = catalog.ensure_sidecar_import_batch(*photo_id, path, &batch_uuid) {
                eprintln!(
                    "bundle import: couldn't record ImportBatch sidecar debt for {}: {e}",
                    path.display()
                );
            }
        }
    }

    // Step C.1 — #148: a new photo's IPTC came from the manifest, and the sidecar beside it
    // (the bundle's own, or a bare identity sidecar) need not carry it. `set_iptc_carried`
    // owed the fields that sidecar has no value for; write them now. A failure leaves them owed for
    // the repair pass rather than being logged and forgotten.
    for &photo_id in &newly_created {
        match catalog.write_owed_iptc(photo_id) {
            Ok(Some((crate::catalog::IptcSettled::Failed(e), _))) => {
                eprintln!("bundle import: IPTC sidecar write for photo {photo_id} owed for repair: {e}")
            }
            Ok(_) => {}
            Err(e) => eprintln!("bundle import: couldn't write owed IPTC for photo {photo_id}: {e}"),
        }
    }

    // Step D — #185: an existing photo's blank IPTC fields take the bundle's values, through
    // the sidecar-safe store (`fill_blank_iptc`).
    for (photo_id, offered) in &merged.iptc_fills {
        fill_blank_iptc(catalog, *photo_id, offered);
    }

    // A version added to a photo is a version write, and owes the monochrome refresh every
    // such write owes (docs/editing.md): a B&W version marks the photo monochrome. Adding
    // versions never removes one, so the flag is only ever set here, never cleared.
    #[cfg(feature = "edit")]
    for &photo_id in &versions_added_to {
        let any_bw = catalog
            .list_versions(photo_id)
            .is_ok_and(|vs| vs.iter().any(|v| crate::plugins::edit::is_bw(&v.edit_json)));
        if any_bw {
            let _ = catalog.set_grayscale(photo_id, true);
        }
    }
    #[cfg(not(feature = "edit"))]
    let _ = versions_added_to;

    // Apply auto-tags (monochrome, long-exposure, etc.) and pair RAW+JPEG stacks.
    let _ = catalog.apply_auto_tags();
    let _ = catalog.pair_raw_jpeg_stacks();
    // The WHOLE-catalog reconcile, deliberately: a bundle merge can insert metadata-only
    // rows anywhere in the library (photos whose originals weren't in the bundle), so
    // there is no folder that bounds the affected set. Scans use the scoped
    // `reconcile_missing_for` instead — see `scanner::phase_b_enrich`.
    let _ = catalog.reconcile_missing();

    // The merge counts the photos Step A created as "existing" — it ran after them. To the
    // user they are what this import added, so report them so: `photos_added` is everything
    // new to this catalog (created by the upsert, plus metadata-only photos the merge
    // inserted), and `photos_existing` only what was here before.
    let mut merge_summary = merge_summary;
    merge_summary.photos_added += newly_created.len();
    merge_summary.photos_existing = merge_summary.photos_existing.saturating_sub(newly_created.len());
    merge_summary.photos_matched_by_capture = matched_by_capture.len();
    partial_result.merge = merge_summary;
    Ok(Indexed { result: partial_result, indexed, total })
}

/// The identity of the row at `path` when it is not the bundle photo's (`bundle_uuid`, as
/// [`crate::catalog::photo_identity_for`] reads it), else `None` — also when no row is
/// there, or the bundle photo has no identity (a pre-#146 blank uuid, which the importer
/// resolves to the row at its path, #150).
fn held_by_another_identity(catalog: &Catalog, path: &Path, bundle_uuid: &str) -> Result<Option<String>, String> {
    use rusqlite::OptionalExtension;
    let Some(identity) = crate::catalog::photo_identity_for(bundle_uuid) else { return Ok(None) };
    // Outside the root: the upsert reports that, per photo.
    let Ok(relative) = catalog.to_relative(path) else { return Ok(None) };
    let held: Option<String> = catalog
        .conn()
        .query_row("SELECT uuid FROM photos WHERE path = ?1", [&relative], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(held.filter(|uuid| *uuid != identity))
}

/// Fill an existing photo's blank IPTC fields with the bundle's `offered` values (#185):
/// only a field that neither the row nor the sidecar beside its original has a value for —
/// a value either already holds is the photo's own and wins. An existing row's IPTC changes
/// only through [`Catalog::set_iptc`] (AGENTS.md, "XMP safety"): the original's path is
/// resolved first, so an unreachable original changes nothing; the store happens under the
/// sidecar's write turn ([`WriteOrder`](crate::xmp::lock::WriteOrder)), held through the
/// write and the compare-and-set settle; a write that fails stays owed for the repair pass.
/// Not `set_iptc_carried`: that one is for values arriving beside a sidecar of their own,
/// and the sidecar here is the existing photo's, not the bundle's.
///
/// A sidecar that does not parse says nothing about what it holds, so nothing is filled:
/// the owed write would later land on whatever it carries (when uncertain, preserve).
///
/// Blocking (sidecar IO and the turn); runs with no transaction open on `catalog`, so a
/// store waiting on another writer's turn never holds the catalog's write lock.
fn fill_blank_iptc(catalog: &Catalog, photo_id: i64, offered: &crate::catalog::IptcFields) {
    use crate::catalog::IptcMask;
    use crate::xmp::lock::WriteOrder;

    let resolve = || catalog.resolve_photo_path(photo_id).ok().flatten();
    let Some(first) = resolve() else {
        eprintln!("bundle import: photo {photo_id}'s original is unreachable; its blank IPTC is not filled");
        return;
    };
    let mut turn = WriteOrder::reserve(&first).wait();
    // The original may resolve to another copy once the turn is ours (another location came
    // back): follow it, as the IPTC save does (`app::iptc::run_in_turn`).
    let mut original = None;
    for _ in 0..=3 {
        let Some(now) = resolve() else {
            eprintln!("bundle import: photo {photo_id}'s original went away; its blank IPTC is not filled");
            return;
        };
        match turn.moved_to(&now) {
            None => {
                original = Some(now);
                break;
            }
            Some(next) => {
                drop(turn);
                turn = next.wait();
            }
        }
    }
    let Some(original) = original else {
        eprintln!("bundle import: photo {photo_id}'s original kept moving; its blank IPTC is not filled");
        return;
    };
    let in_sidecar = match crate::xmp::read_iptc_present(&original) {
        Ok(present) => present,
        Err(e) => {
            eprintln!("bundle import: photo {photo_id}'s sidecar does not parse ({e}); its blank IPTC is not filled");
            return;
        }
    };
    let Ok(current) = catalog.get_iptc(photo_id) else { return };
    let fill = IptcMask::present_in(offered)
        .without(IptcMask::present_in(&current))
        .without(in_sidecar);
    if fill.is_empty() {
        return;
    }
    let mut next = current;
    for m in IptcMask::EACH.into_iter().filter(|m| fill.contains(*m)) {
        if let Some(slot) = m.value_mut(&mut next) {
            *slot = m.value(offered).to_string();
        }
    }
    let write = match catalog.set_iptc(photo_id, &next) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("bundle import: couldn't fill photo {photo_id}'s blank IPTC: {e}");
            return;
        }
    };
    let outcome = write.run(&original);
    match catalog.settle_iptc_write(&write, &outcome) {
        Ok(crate::catalog::IptcSettled::Failed(e)) => {
            eprintln!("bundle import: IPTC sidecar write for photo {photo_id} owed for repair: {e}")
        }
        Ok(_) => {}
        Err(e) => eprintln!("bundle import: couldn't record the IPTC sidecar write for photo {photo_id}: {e}"),
    }
    drop(turn);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------


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

    fn temp_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(&format!("import-{tag}"))
    }

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = temp_dir(tag);
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    /// A catalog of no photos rooted at `root`, for an unpack test that indexes nothing: its
    /// database is a hidden file in `root`, which no unpack touches.
    fn empty_catalog(root: &Path) -> Catalog {
        std::fs::create_dir_all(root).unwrap();
        Catalog::open(&root.join(".test.chairphoto"), root).unwrap()
    }

    /// Build a minimal bundle zip with one "original" file (fake bytes) and write it
    /// to a temp path.
    fn make_test_bundle(
        tag: &str,
        photo_uuid: &str,
        relative_path: &str,
    ) -> crate::test_support::TestSubPath {
        let iptc = crate::catalog::IptcFields { headline: "Test sunset".into(), ..Default::default() };
        make_test_bundle_with(tag, photo_uuid, relative_path, iptc, None)
    }

    /// [`make_test_bundle`] with the manifest's IPTC and, when given, a sidecar carried in
    /// the bundle beside the original.
    fn make_test_bundle_with(
        tag: &str,
        photo_uuid: &str,
        relative_path: &str,
        iptc: crate::catalog::IptcFields,
        sidecar: Option<&str>,
    ) -> crate::test_support::TestSubPath {
        use crate::bundle::BundlePhoto;
        use crate::catalog::PickState;

        let dir = temp_dir(&format!("{tag}-bundle"));

        // Create the fake original file so the writer can copy it.
        let orig_dir = dir.join("orig");
        std::fs::create_dir_all(&orig_dir).unwrap();
        let orig_path = orig_dir.join("DSC01234.ARW");
        std::fs::write(&orig_path, b"FAKE RAW BYTES").unwrap();
        if let Some(xml) = sidecar {
            std::fs::write(crate::xmp::sidecar_path(&orig_path), xml).unwrap();
        }

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
            iptc,
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        });

        let mut originals = HashMap::new();
        originals.insert(photo_uuid.to_string(), Some(orig_path));

        let bundle = GatheredBundle { manifest, originals };
        let dest = dir.join("test.chairphoto");
        write_bundle(&bundle, &dest, |_, _| {}).expect("write_bundle");
        dir.into_subpath("test.chairphoto")
    }

    /// A bundle of `photos`, each with its original copied from the given file (none: a
    /// metadata-only entry).
    fn bundle_of(
        tag: &str,
        photos: Vec<(crate::bundle::BundlePhoto, Option<PathBuf>)>,
    ) -> crate::test_support::TestSubPath {
        let dir = temp_dir(&format!("{tag}-bundle"));
        let mut manifest = BundleManifest::new(
            BundleBatch {
                uuid: format!("batch-{tag}"),
                source_label: "Test batch".into(),
                note: String::new(),
                created_at: 1_700_000_000,
            },
            1_700_100_000,
        );
        let mut originals = HashMap::new();
        for (bp, original) in photos {
            originals.insert(bp.uuid.clone(), original);
            manifest.photos.push(bp);
        }
        let dest = dir.join("test.chairphoto");
        write_bundle(&GatheredBundle { manifest, originals }, &dest, |_, _| {}).expect("write_bundle");
        dir.into_subpath("test.chairphoto")
    }

    /// A bundle photo with no culling, IPTC, edits or tags of its own.
    fn plain_photo(uuid: &str, relative_path: &str) -> crate::bundle::BundlePhoto {
        crate::bundle::BundlePhoto {
            uuid: uuid.into(),
            relative_path: relative_path.into(),
            rating: 0,
            label: String::new(),
            pick_state: crate::catalog::PickState::None,
            iptc: Default::default(),
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        }
    }

    // --- same name, same size (#246) ----------------------------------------------------

    /// The library holds `DSC1.jpg`; the bundle brings two photos of that name and size taken
    /// in the same second — one the same capture (skipped), one another body's (kept as
    /// ` (2)`, its own row and the bundle's UUID). The library file is never overwritten, and
    /// importing the bundle again skips both.
    #[test]
    fn a_bundle_original_is_skipped_only_when_it_is_the_same_capture() {
        use crate::scanner::same_photo::test_files::{exiftool_available, stamped_jpeg};
        if !exiftool_available("a_bundle_original_is_skipped_only_when_it_is_the_same_capture") {
            return;
        }
        const SAME: &str = "0b7f3b1e-1111-4c3d-9e8f-0a1b2c3d4e5f";
        const OTHER: &str = "0b7f3b1e-2222-4c3d-9e8f-0a1b2c3d4e5f";
        let src = temp_dir("246-src");
        let same = src.join("same/DSC1.jpg");
        let other = src.join("other/DSC1.jpg");
        stamped_jpeg(&same, "2026:06:28 12:00:00", "123", "4711");
        stamped_jpeg(&other, "2026:06:28 12:00:00", "123", "9999");
        let bundle_path = bundle_of(
            "246",
            vec![
                (plain_photo(SAME, "2026/06/28/DSC1.jpg"), Some(same.clone())),
                (plain_photo(OTHER, "2026/06/29/DSC1.jpg"), Some(other.clone())),
            ],
        );

        let (catalog, root) = temp_catalog("246");
        // The library has the first photo on both days (the second day's under the same
        // name, the same capture as `same`: so `other` collides with a different photo).
        for day in ["28", "29"] {
            let lib = root.join(format!("2026/06/{day}/DSC1.jpg"));
            std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
            std::fs::copy(&same, &lib).unwrap();
        }
        assert_eq!(std::fs::metadata(&same).unwrap().len(), std::fs::metadata(&other).unwrap().len());

        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!((partial.skipped_duplicate, partial.copied, partial.errors), (1, 1, 0));
        let kept = root.join("2026/06/29/DSC1 (2).jpg");
        assert_eq!(std::fs::read(&kept).unwrap(), std::fs::read(&other).unwrap());
        assert_eq!(std::fs::read(root.join("2026/06/29/DSC1.jpg")).unwrap(), std::fs::read(&same).unwrap());
        let hidden: Vec<_> = std::fs::read_dir(root.join("2026/06/29"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".chairphoto-import-"))
            .collect();
        assert!(hidden.is_empty(), "no staged file left behind");
        index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        let other_row = catalog.get_photo_by_uuid(OTHER).unwrap();
        assert_eq!(other_row.path, "2026/06/29/DSC1 (2).jpg");

        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (_, again) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!((again.skipped_duplicate, again.copied), (2, 0), "a second import skips both");
        assert!(!root.join("2026/06/29/DSC1 (3).jpg").exists());
    }

    /// M-a of the second #246 review: an orphan sidecar — another tool's (digiKam's rating,
    /// keywords and identifier), its original gone — sits at the ` (2)` name a different
    /// photo would take. The bundle's original goes to ` (3)` instead, whether or not the
    /// bundle carries a sidecar: the orphan is neither overwritten by the bundle's sidecar nor
    /// adopted (its identity on the new photo), and the bundle's photo gets one row, at its
    /// copy, under its own identity.
    #[test]
    fn an_orphan_sidecar_at_a_free_name_is_neither_overwritten_nor_adopted() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const ORPHAN_ID: &str = "11111111-1111-4111-8111-111111111111";
        let orphan_xml = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:digiKam="http://www.digikam.org/ns/1.0/" xmp:Rating="5" xmp:Identifier="{ORPHAN_ID}"><dc:subject><rdf:Bag><rdf:li>ForeignKeyword</rdf:li></rdf:Bag></dc:subject><digiKam:TagsList><rdf:Seq><rdf:li>Foreign/Keyword</rdf:li></rdf:Seq></digiKam:TagsList></rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let bundle_xml = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Identifier="{THEIRS}"/></rdf:RDF></x:xmpmeta>"#
        );
        for (tag, sidecar) in [("ma-with", Some(bundle_xml.as_str())), ("ma-without", None)] {
            let (catalog, root) = temp_catalog(tag);
            let dir = root.join("2026/06/28");
            std::fs::create_dir_all(&dir).unwrap();
            // Another photo of that name and size (no row), and the orphan at ` (2)`.
            std::fs::write(dir.join("DSC01234.ARW"), b"OTHER RAWBYTES").unwrap();
            let orphan = dir.join("DSC01234 (2).ARW.xmp");
            std::fs::write(&orphan, &orphan_xml).unwrap();

            let bundle_path = make_test_bundle_with(
                tag,
                THEIRS,
                "2026/06/28/DSC01234.ARW",
                Default::default(),
                sidecar,
            );
            let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
            let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
            assert_eq!((partial.copied, partial.skipped_duplicate, partial.errors), (1, 0, 0), "{tag}");
            let placed = dir.join("DSC01234 (3).ARW");
            assert_eq!(std::fs::read(&placed).unwrap(), b"FAKE RAW BYTES", "{tag}");
            assert!(!dir.join("DSC01234 (2).ARW").exists(), "{tag}: nothing placed beside the orphan");
            assert_eq!(std::fs::read_to_string(&orphan).unwrap(), orphan_xml, "{tag}: the orphan is untouched");

            index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
            assert_eq!(std::fs::read_to_string(&orphan).unwrap(), orphan_xml, "{tag}: still untouched");
            assert_eq!(crate::xmp::read_identifier(&placed).as_deref(), Some(THEIRS), "{tag}");
            assert_eq!(catalog.get_photo_by_uuid(THEIRS).unwrap().path, "2026/06/28/DSC01234 (3).ARW", "{tag}");
            assert!(catalog.get_photo_by_uuid(ORPHAN_ID).is_err(), "{tag}: the orphan's identity is no row's");
            assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1, "{tag}: no phantom row");
        }
    }

    /// L-c of the second #246 review: the unpack lists each folder once and records each name
    /// it places. The library holds a `DSC1.ARW`; the bundle's first photo is its own
    /// `DSC1 (2).ARW`, placed at that free name after the folder was listed, and its second
    /// photo, `DSC1.ARW`, is the same file: it collides with the library's (other bytes, same
    /// size) and is found at the ` (2)` this unpack placed — skipped, not copied a third time.
    #[test]
    fn a_name_placed_earlier_in_the_unpack_is_a_candidate() {
        let src = temp_dir("lc-src");
        let (first, second) = (src.join("a/DSC1 (2).ARW"), src.join("b/DSC1.ARW"));
        for p in [&first, &second] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"FAKE RAW TWO").unwrap();
        }
        let bundle_path = bundle_of(
            "lc",
            vec![
                (plain_photo("uuid-lc-1", "2026/06/28/DSC1 (2).ARW"), Some(first)),
                (plain_photo("uuid-lc-2", "2026/06/28/DSC1.ARW"), Some(second)),
            ],
        );
        let dest_base = temp_dir("lc-dest");
        let day = dest_base.join("2026/06/28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("DSC1.ARW"), b"FAKE RAW ONE").unwrap();
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (_, partial) = extract_originals(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, |_, _| {}).unwrap();
        assert_eq!((partial.copied, partial.skipped_duplicate), (1, 1), "{partial:?}");
        assert_eq!(std::fs::read(day.join("DSC1 (2).ARW")).unwrap(), b"FAKE RAW TWO");
        assert_eq!(std::fs::read(day.join("DSC1.ARW")).unwrap(), b"FAKE RAW ONE");
        assert!(!day.join("DSC1 (3).ARW").exists());
    }

    /// N-3 of the third #246 review: the folder is listed once for the unpack, so a ` (n)`
    /// another program writes there meanwhile used to be no candidate, and the same photo was
    /// copied once more at the next ` (n)`. The listing is renewed right before a copy: the
    /// photo written there meanwhile is found and skipped.
    #[test]
    fn a_numbered_name_written_during_the_unpack_is_a_candidate() {
        let src = temp_dir("n3-src");
        let (first, second) = (src.join("A.ARW"), src.join("DSC1.ARW"));
        std::fs::write(&first, b"another photo").unwrap();
        std::fs::write(&second, b"FAKE RAW TWO").unwrap();
        let bundle_path = bundle_of(
            "n3",
            vec![
                (plain_photo("uuid-n3-1", "2026/06/28/A.ARW"), Some(first)),
                (plain_photo("uuid-n3-2", "2026/06/28/DSC1.ARW"), Some(second)),
            ],
        );
        let dest_base = temp_dir("n3-dest");
        let day = dest_base.join("2026/06/28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("DSC1.ARW"), b"FAKE RAW ONE").unwrap();
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        // Before the second original (the folder listed for the first), another import writes
        // the same photo at ` (2)`.
        let meanwhile = |done: usize, _| {
            if done == 2 {
                std::fs::write(day.join("DSC1 (2).ARW"), b"FAKE RAW TWO").unwrap();
            }
        };
        let (_, partial) = extract_originals(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, meanwhile).unwrap();
        assert_eq!((partial.copied, partial.skipped_duplicate), (1, 1), "{partial:?}");
        assert!(!day.join("DSC1 (3).ARW").exists(), "not copied a second time");
        assert_eq!(std::fs::read(day.join("DSC1.ARW")).unwrap(), b"FAKE RAW ONE");
    }

    /// A sidecar that appears at the name after the original's place was chosen (another
    /// program wrote it meanwhile) is never replaced by the bundle's: it is left as it is.
    #[test]
    fn the_bundles_sidecar_never_replaces_a_file() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        let bundle_xml = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Identifier="{THEIRS}"/></rdf:RDF></x:xmpmeta>"#
        );
        let bundle_path = make_test_bundle_with(
            "ma-race",
            THEIRS,
            "2026/06/28/DSC01234.ARW",
            Default::default(),
            Some(&bundle_xml),
        );
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let dir = temp_dir("ma-race-dest");
        let dest = dir.join("DSC01234.ARW");
        std::fs::write(&dest, b"FAKE RAW BYTES").unwrap();
        std::fs::write(crate::xmp::sidecar_path(&dest), b"written meanwhile").unwrap();
        let arc_orig = format!("{ORIGINALS_DIR}/2026/06/28/DSC01234.ARW");
        place_sidecar(&mut archive, &arc_orig, &dest, &manifest.photos[0]);
        assert_eq!(std::fs::read(crate::xmp::sidecar_path(&dest)).unwrap(), b"written meanwhile");
    }

    /// An abort while a collision is being decided copies nothing of it and leaves nothing
    /// beside the library's files; an original decided before the stop stays placed.
    #[test]
    fn an_abort_during_a_collision_copies_nothing_of_it() {
        let src = temp_dir("246-abort-src");
        let (a, b) = (src.join("a/DSC1.ARW"), src.join("b/DSC2.ARW"));
        for p in [&a, &b] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"FAKE RAW BYTES").unwrap();
        }
        let bundle_path = bundle_of(
            "246-abort",
            vec![
                (plain_photo("uuid-246-a", "2026/06/28/DSC1.ARW"), Some(a)),
                (plain_photo("uuid-246-b", "2026/06/28/DSC2.ARW"), Some(b)),
            ],
        );
        // Abort as the first original's collision is decided, and as the second's.
        for (stop_after, placed) in [(1, &[][..]), (2, &["DSC1 (2).ARW", "DSC1 (2).ARW.xmp"][..])] {
            let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
            let dest_base = temp_dir("246-abort-dest");
            let date_dir = dest_base.join("2026/06/28");
            std::fs::create_dir_all(&date_dir).unwrap();
            // Same size as the bundle's "FAKE RAW BYTES", other bytes: both collide.
            for name in ["DSC1.ARW", "DSC2.ARW"] {
                std::fs::write(date_dir.join(name), b"OTHER RAWBYTES").unwrap();
            }
            let abort = AtomicBool::new(false);
            let (_, partial, aborted) =
                extract_originals_abortable(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, &abort, |done, _| {
                    if done == stop_after {
                        abort.store(true, Ordering::Relaxed)
                    }
                })
                .unwrap();
            assert!(aborted && partial.copied == stop_after - 1, "stop after {stop_after}: {partial:?}");
            let mut names: Vec<String> = std::fs::read_dir(&date_dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let mut expected: Vec<&str> = ["DSC1.ARW", "DSC2.ARW"].into_iter().chain(placed.iter().copied()).collect();
            expected.sort();
            assert_eq!(names, expected, "stop after {stop_after}");
        }
    }

    /// Every file and directory under `root`, with its size and modification time: a
    /// directory's mtime moves when an entry is created or removed in it, so a file written
    /// and removed again between two snapshots still shows.
    fn tree_snapshot(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .map(|e| e.unwrap())
            .map(|e| {
                let md = e.metadata().unwrap();
                (e.path().to_path_buf(), md.len(), md.modified().unwrap())
            })
            .collect();
        out.sort();
        out
    }

    /// M-1 of the #246 review: re-importing a bundle the library already holds writes nothing
    /// to the library's disk — no copy of an original is unpacked there to be compared, at
    /// any point of the unpack — and skips every original.
    #[test]
    fn re_importing_a_bundle_writes_nothing_to_the_library() {
        let src = temp_dir("246-again-src");
        let mut photos = Vec::new();
        for (i, name) in ["DSC1.ARW", "DSC2.ARW", "DSC3.ARW"].into_iter().enumerate() {
            let p = src.join(name);
            std::fs::write(&p, format!("FAKE RAW BYTES {i}")).unwrap();
            photos.push((plain_photo(&format!("uuid-246-again-{i}"), &format!("2026/06/28/{name}")), Some(p)));
        }
        let bundle_path = bundle_of("246-again", photos);
        let (catalog, root) = temp_catalog("246-again");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.copied, 3);
        index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();

        let before = tree_snapshot(&root);
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (_, again) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {
            assert_eq!(tree_snapshot(&root), before, "the library is unchanged during the unpack");
        })
        .unwrap();
        assert_eq!((again.skipped_duplicate, again.copied, again.errors), (3, 0, 0));
        assert_eq!(tree_snapshot(&root), before, "the library is unchanged after the unpack");
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
            extract_originals(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, |_, _| {})
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
            extract_originals(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, |_, _| {})
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
            extract_originals(&empty_catalog(&dest_base), &manifest, &mut archive, &dest_base, |_, _| {})
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
        let dest_base = root.to_path_buf();

        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &dest_base, |_, _| {})
                .expect("extract_originals");

        let result = index_bundle(&catalog, &manifest, &extracted, &dest_base, partial)
            .expect("index_bundle");

        // The copy phase copied one file with no errors.
        assert_eq!(result.copied, 1);
        assert_eq!(result.errors, 0);

        // The F1c merge adds the batch. The upsert phase created the photo before the merge
        // ran; the result reports it as what this import added (not as existing), and the
        // merge did not duplicate it.
        assert!(result.merge.batch_added, "batch must be recorded");
        assert_eq!(result.merge.photos_existing, 0, "nothing was here before");
        assert_eq!(result.merge.photos_added, 1, "the import added one photo");
        assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1, "and only one");

        // The photo is in the catalog with the bundle's state (rating applied by merge).
        let photo = catalog.get_photo_by_uuid("uuid-index-1").unwrap();
        assert_eq!(photo.rating, 3);
        let iptc = catalog.get_iptc(photo.id).unwrap();
        assert_eq!(iptc.headline, "Test sunset");
    }

    /// #148 (review of #144, L1): the bundle's IPTC reaches the catalog and, from there, the
    /// sidecar beside the extracted original — which the bundle need not have carried it in.
    #[test]
    fn index_bundle_writes_the_bundles_iptc_into_the_sidecar() {
        let bundle_path = make_test_bundle("iptc-148", "uuid-iptc-148", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("iptc-148");
        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).expect("extract_originals");
        index_bundle(&catalog, &manifest, &extracted, &root, partial).expect("index_bundle");

        let photo = catalog.get_photo_by_uuid(&crate::catalog::photo_identity_for("uuid-iptc-148").unwrap()).unwrap();
        let xmp = crate::xmp::sidecar_path(&catalog.require_photo_path(photo.id).unwrap());
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert!(xml.contains("Test sunset"), "the bundle's headline is in the sidecar:\n{xml}");
        assert_eq!(catalog.owed_iptc(photo.id).unwrap(), crate::catalog::IptcMask::NONE);
    }

    /// Review of #148, M1: a value the bundle's own sidecar carries is another tool's (here
    /// Lightroom changed the Headline after ChairPhoto wrote it, so the sidecar has
    /// `chairphoto:LastWrite` and no backup would be made). The import keeps it, and owes
    /// the sidecar only the manifest fields it has no value for.
    #[test]
    fn index_bundle_keeps_a_value_the_bundled_sidecar_carries() {
        use crate::xmp::test_fixtures::property_values;
        let sidecar = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/" xmlns:chairphoto="https://chairphoto.local/ns/1.0/" photoshop:Headline="LR"><chairphoto:LastWrite>1700000000</chairphoto:LastWrite></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let iptc = crate::catalog::IptcFields { headline: "A".into(), city: "Oslo".into(), ..Default::default() };
        let bundle_path =
            make_test_bundle_with("iptc-148-m1", "uuid-iptc-148-m1", "2026/06/28/DSC01234.ARW", iptc, Some(sidecar));
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("iptc-148-m1");
        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).expect("extract_originals");
        index_bundle(&catalog, &manifest, &extracted, &root, partial).expect("index_bundle");

        let photo =
            catalog.get_photo_by_uuid(&crate::catalog::photo_identity_for("uuid-iptc-148-m1").unwrap()).unwrap();
        let xmp = crate::xmp::sidecar_path(&catalog.require_photo_path(photo.id).unwrap());
        let xml = std::fs::read_to_string(&xmp).unwrap();
        let photoshop = "http://ns.adobe.com/photoshop/1.0/";
        assert_eq!(property_values(&xml, photoshop, "Headline"), vec!["LR"], "another tool's value survives:\n{xml}");
        assert_eq!(property_values(&xml, photoshop, "City"), vec!["Oslo"], "a field the sidecar lacked is written:\n{xml}");
        assert_eq!(catalog.get_iptc(photo.id).unwrap().headline, "A", "the catalog keeps the manifest's value");
        assert_eq!(catalog.owed_iptc(photo.id).unwrap(), crate::catalog::IptcMask::NONE);
    }

    /// A bundle photo whose sidecar cannot take its IPTC (here: an unparseable sidecar in the
    /// bundle) is left owing it, and the repair pass writes it once the sidecar is fixed.
    #[test]
    fn index_bundle_owes_iptc_a_sidecar_could_not_take() {
        let bundle_path = make_test_bundle("iptc-148-owed", "uuid-iptc-148-owed", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("iptc-148-owed");
        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).expect("extract_originals");
        let xmp = crate::xmp::sidecar_path(&extracted[0].dest);
        std::fs::write(&xmp, "<x:xmpmeta not xml").unwrap();
        index_bundle(&catalog, &manifest, &extracted, &root, partial).expect("index_bundle");

        let photo = catalog.get_photo_by_uuid(&crate::catalog::photo_identity_for("uuid-iptc-148-owed").unwrap()).unwrap();
        assert_eq!(catalog.owed_iptc(photo.id).unwrap(), crate::catalog::IptcMask::HEADLINE);
        assert_eq!(catalog.summarize_pending_identity().unwrap().iptc_owed, 1);

        std::fs::remove_file(&xmp).unwrap();
        let summary = catalog.repair_pending_identity().unwrap();
        assert_eq!(summary.iptc_written, 1, "{summary:?}");
        assert!(std::fs::read_to_string(&xmp).unwrap().contains("Test sunset"));
        assert_eq!(catalog.owed_iptc(photo.id).unwrap(), crate::catalog::IptcMask::NONE);
    }

    #[test]
    fn re_import_is_idempotent() {
        let bundle_path =
            make_test_bundle("idem", "uuid-idem-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("idem");

        // First import.
        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {})
                .expect("first extract");
        index_bundle(&catalog, &manifest, &extracted, &root, partial)
            .expect("first index");

        // Re-open the bundle for the second import.
        let (manifest2, mut archive2) = open_bundle(&bundle_path).expect("open_bundle 2");
        let (extracted2, partial2) =
            extract_originals(&catalog, &manifest2, &mut archive2, &root, |_, _| {})
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

    /// #248: values the user clears after a bundle import — culling and IPTC the bundle gave
    /// a new photo — stay cleared when the same bundle is imported again.
    #[test]
    fn a_second_import_of_a_bundle_brings_back_nothing_the_user_cleared() {
        use crate::catalog::{IptcFields, PickState};
        let bundle_path = make_test_bundle("248", "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f", "2026/06/28/DSC01234.ARW");
        let (catalog, root) = temp_catalog("248");
        let import = || {
            let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
            let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
            index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap()
        };
        import();
        let id = catalog.get_photo_by_uuid("6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f").unwrap().id;
        assert_eq!(catalog.get_photo(id).unwrap().rating, 3);
        assert_eq!(catalog.get_iptc(id).unwrap().headline, "Test sunset");

        catalog.set_culling(id, Some(0), Some(""), Some(PickState::None)).unwrap();
        catalog.set_iptc(id, &IptcFields::default()).unwrap();
        catalog.write_owed_iptc(id).unwrap();

        let again = import();
        assert_eq!((again.merge.photos_merged_before, again.merge.photos_filled), (1, 0), "{:?}", again.merge);
        let after = catalog.get_photo(id).unwrap();
        assert_eq!((after.rating, after.label.as_str(), after.pick_state), (0, "", PickState::None));
        assert_eq!(catalog.get_iptc(id).unwrap(), IptcFields::default(), "the cleared headline stays cleared");
    }

    // --- identity re-homes (#150) ------------------------------------------------------

    /// #150 (review N2 of #146): a bundle carrying a photo this catalog already has, at
    /// another relative path, must not pull the row off the original, which is still in
    /// place. Before the fix the row was re-homed onto the bundle's copy and the next rescan
    /// catalogued the original afresh, with none of its rating.
    #[test]
    fn a_bundle_copy_at_another_path_leaves_the_original_its_row() {
        const KNOWN: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        let bundle_path = make_test_bundle("n2", KNOWN, "2021/02/02/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");
        let (catalog, root) = temp_catalog("n2");
        let original = root.join("2020/01/01/DSC01234.ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();
        let row = catalog.upsert_photo_with_identity(&original, None, 1, 14, Some(KNOWN)).unwrap();
        catalog.set_culling(row.id, Some(5), None, None).unwrap();

        let (extracted, partial) =
            extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).expect("extract");
        index_bundle(&catalog, &manifest, &extracted, &root, partial).expect("index");

        let kept = catalog.get_photo(row.id).unwrap();
        assert_eq!((kept.path.as_str(), kept.rating, kept.uuid.as_str()),
            ("2020/01/01/DSC01234.ARW", 5, KNOWN), "the original keeps its row");
        let copy: (i64, String) = catalog
            .conn()
            .query_row(
                "SELECT id, uuid FROM photos WHERE path = '2021/02/02/DSC01234.ARW'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_ne!(copy.1, KNOWN, "the bundle's copy has a row of its own");
        let queued: String = catalog
            .conn()
            .query_row(
                "SELECT error FROM pending_sidecar_identity WHERE photo_id = ?1 AND field = 'identifier'",
                [copy.0],
                |r| r.get(0),
            )
            .unwrap();
        assert!(queued.contains(KNOWN), "its sidecar's identity is reported: {queued}");

        // #185: the bundle's state goes onto the row merge matches by identity — filling in
        // only what it lacks — and none of it onto the copy's row.
        assert_eq!((kept.label.as_str(), kept.pick_state), ("green", crate::catalog::PickState::Pick));
        assert_eq!(catalog.get_iptc(row.id).unwrap().headline, "Test sunset");
        let copy_row = catalog.get_photo(copy.0).unwrap();
        assert_eq!((copy_row.rating, copy_row.label.as_str()), (0, ""), "the copy gets no culling");
        assert_eq!(catalog.get_iptc(copy.0).unwrap(), Default::default(), "nor IPTC");
    }

    // --- a bundle photo the library already has (#185) ------------------------------------

    /// The bundle brings a photo the library already has, edited, at the same path: the file
    /// is skipped (#246) and the bundle's data lands on the existing row — its edit record
    /// and version as new versions after the row's own, its label and pick where the row had
    /// none, its IPTC only where neither the row nor the row's sidecar has a value (written
    /// to that sidecar, nothing left owed). The row's own values win; no second row, no
    /// backup queued and no batch membership for a photo this import did not add.
    #[test]
    fn a_bundle_of_an_existing_edited_photo_adds_versions_and_fills_only_blanks() {
        use crate::bundle::BundleVersion;
        use crate::catalog::{IptcFields, IptcMask, PickState};
        const KNOWN: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        let (catalog, root) = temp_catalog("185");
        let original = root.join("2026/06/28/DSC01234.ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();
        // The row's sidecar holds a Country the catalog never imported: the photo's own value.
        std::fs::write(
            crate::xmp::sidecar_path(&original),
            format!(
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/" photoshop:Country="Norway"><xmp:Identifier><rdf:Bag><rdf:li>{KNOWN}</rdf:li></rdf:Bag></xmp:Identifier></rdf:Description></rdf:RDF></x:xmpmeta>"#
            ),
        )
        .unwrap();
        let row = catalog.upsert_photo_with_identity(&original, None, 1, 14, Some(KNOWN)).unwrap();
        catalog.set_culling(row.id, Some(4), None, None).unwrap();
        catalog.set_iptc(row.id, &IptcFields { headline: "Mine".into(), ..Default::default() }).unwrap();
        catalog.write_owed_iptc(row.id).unwrap();
        let mine = catalog.create_version(row.id, "Mine").unwrap();
        catalog.set_version_edit(mine, r#"{"local":1}"#).unwrap();

        let src = temp_dir("185-src");
        let file = src.join("DSC01234.ARW");
        std::fs::write(&file, b"FAKE RAW BYTES").unwrap();
        let mut bp = plain_photo(KNOWN, "2026/06/28/DSC01234.ARW");
        bp.rating = 3;
        bp.label = "green".into();
        bp.pick_state = PickState::Pick;
        bp.iptc = IptcFields {
            headline: "Theirs".into(),
            city: "Oslo".into(),
            country: "Sweden".into(),
            ..Default::default()
        };
        bp.edit_record = Some(r#"{"bw":{"enabled":true}}"#.into());
        bp.versions = vec![BundleVersion { name: "Square".into(), edit_json: r#"{"crop":"1:1"}"#.into(), position: 0 }];
        let bundle_path = bundle_of("185", vec![(bp, Some(file))]);

        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!((partial.skipped_duplicate, partial.copied), (1, 0), "the file is already here");
        let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        let m = &result.merge;
        assert_eq!((m.photos_added, m.photos_existing, m.photos_filled, m.versions_added), (0, 1, 1, 2), "{m:?}");
        assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1, "no second row");

        let after = catalog.get_photo(row.id).unwrap();
        assert_eq!((after.rating, after.label.as_str(), after.pick_state), (4, "green", PickState::Pick));
        let iptc = catalog.get_iptc(row.id).unwrap();
        assert_eq!(
            (iptc.headline.as_str(), iptc.city.as_str(), iptc.country.as_str()),
            ("Mine", "Oslo", ""),
            "the row's headline and its sidecar's country win; the blank city is filled"
        );
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&original)).unwrap();
        use crate::xmp::test_fixtures::property_values;
        let photoshop = "http://ns.adobe.com/photoshop/1.0/";
        assert_eq!(property_values(&xml, photoshop, "City"), vec!["Oslo"], "{xml}");
        assert_eq!(property_values(&xml, photoshop, "Headline"), vec!["Mine"], "{xml}");
        assert_eq!(property_values(&xml, photoshop, "Country"), vec!["Norway"], "{xml}");
        assert_eq!(catalog.owed_iptc(row.id).unwrap(), IptcMask::NONE);

        let versions: Vec<(String, String)> =
            catalog.list_versions(row.id).unwrap().into_iter().map(|v| (v.name, v.edit_json)).collect();
        assert_eq!(
            versions,
            [
                ("Mine".to_string(), r#"{"local":1}"#.to_string()),
                (crate::catalog::IMPORTED_EDIT_VERSION.to_string(), r#"{"bw":{"enabled":true}}"#.to_string()),
                ("Square".to_string(), r#"{"crop":"1:1"}"#.to_string()),
            ]
        );
        assert_eq!(catalog.get_edit_record(row.id).unwrap(), None, "its edit record is not changed");
        // The B&W version it gained owes the monochrome refresh a version write owes.
        #[cfg(feature = "edit")]
        assert!(catalog.is_grayscale(row.id).unwrap(), "a B&W version marks it monochrome");
        assert!(catalog.list_pending_operations().unwrap().is_empty(), "no backup queued for it");
        assert_eq!(catalog.import_batch_uuid_for_photo(row.id).unwrap(), None, "not in the bundle's batch");

        // Importing the bundle again changes nothing more.
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        let again = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!((again.merge.photos_filled, again.merge.versions_added), (0, 0));
        assert_eq!(catalog.list_versions(row.id).unwrap().len(), 3);
    }

    /// The file is the same capture as the library's, but the library's row has another
    /// identity (both sides imported the card on their own): the bundle's photo is kept apart
    /// — counted, its data on no row — and the library's row and sidecar are untouched.
    #[test]
    fn the_same_file_under_another_identity_is_kept_apart() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const OURS: &str = "7a2d2f1f-3c8b-4d4e-8f90-1b2c3d4e5f60";
        let (catalog, root) = temp_catalog("185-apart");
        let original = root.join("2026/06/28/DSC01234.ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();
        crate::xmp::write_identifier(&original, OURS).unwrap();
        let row = catalog.upsert_photo_with_identity(&original, None, 1, 14, Some(OURS)).unwrap();

        let bundle_path = make_test_bundle("185-apart", THEIRS, "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.skipped_duplicate, 1);
        let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).expect("not a UNIQUE failure");
        assert_eq!((result.merge.photos_kept_apart, result.merge.photos_added), (1, 0), "{:?}", result.merge);

        assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1);
        let after = catalog.get_photo(row.id).unwrap();
        assert_eq!((after.uuid.as_str(), after.rating, after.label.as_str()), (OURS, 0, ""));
        assert_eq!(catalog.get_iptc(row.id).unwrap(), Default::default());
        assert_eq!(crate::xmp::read_identifier(&original).as_deref(), Some(OURS));
    }

    /// #249 (decision 2026-10-06): the card was imported on both machines, each minting its
    /// own identity. When the capture stamps prove the same capture by the strict re-link rule
    /// (a sub-second on both sides here), the bundle's data goes onto the library's row — its
    /// blank culling filled, its versions added — and the row keeps its identity, in the
    /// catalog and in its sidecar; no row is made for the bundle's identity, and a second
    /// import fills nothing (#248). Without that proof (no sub-second, no serial) the photo
    /// stays kept apart, and the result names it.
    #[test]
    fn the_same_capture_under_another_identity_is_merged_when_proven() {
        use crate::bundle::BundleVersion;
        use crate::scanner::same_photo::test_files::{exiftool_available, stamped_jpeg};
        if !exiftool_available("the_same_capture_under_another_identity_is_merged_when_proven") {
            return;
        }
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const OURS: &str = "7a2d2f1f-3c8b-4d4e-8f90-1b2c3d4e5f60";
        for proven in [true, false] {
            let tag = format!("249-{proven}");
            let src = temp_dir(&format!("{tag}-src")).join("DSC1.jpg");
            let (subsec, serial) = if proven { ("123", "") } else { ("", "") };
            stamped_jpeg(&src, "2026:06:28 12:00:00", subsec, serial);
            let (catalog, root) = temp_catalog(&tag);
            let lib = root.join("2026/06/28/DSC1.jpg");
            std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
            std::fs::copy(&src, &lib).unwrap();
            crate::xmp::write_identifier(&lib, OURS).unwrap();
            let len = std::fs::metadata(&lib).unwrap().len() as i64;
            let row = catalog.upsert_photo_with_identity(&lib, None, 1, len, Some(OURS)).unwrap().id;

            let mut bp = plain_photo(THEIRS, "2026/06/28/DSC1.jpg");
            bp.rating = 4;
            bp.label = "green".into();
            bp.versions = vec![BundleVersion { name: "Square".into(), edit_json: r#"{"crop":"1:1"}"#.into(), position: 0 }];
            let bundle_path = bundle_of(&tag, vec![(bp, Some(src.clone()))]);
            let import = || {
                let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
                let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
                assert_eq!((partial.skipped_duplicate, partial.copied), (1, 0), "{proven}");
                index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap()
            };
            let result = import();
            let m = &result.merge;
            let after = catalog.get_photo(row).unwrap();
            assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1, "{proven}: no second row");
            assert!(catalog.get_photo_by_uuid(THEIRS).is_err(), "{proven}: no row of the bundle's identity");
            assert_eq!(after.uuid, OURS, "{proven}: the row keeps its identity");
            assert_eq!(crate::xmp::read_identifier(&lib).as_deref(), Some(OURS), "{proven}: and so does its sidecar");
            if proven {
                assert_eq!((m.photos_matched_by_capture, m.photos_kept_apart, m.photos_filled, m.versions_added), (1, 0, 1, 1), "{m:?}");
                assert_eq!((after.rating, after.label.as_str()), (4, "green"));
                assert_eq!(catalog.list_versions(row).unwrap().len(), 1);
                catalog.set_culling(row, Some(0), None, None).unwrap();
                let again = import();
                assert_eq!((again.merge.photos_merged_before, again.merge.photos_filled), (1, 0), "{:?}", again.merge);
                assert_eq!(catalog.get_photo(row).unwrap().rating, 0, "a second import fills nothing");
            } else {
                assert_eq!((m.photos_matched_by_capture, m.photos_kept_apart), (0, 1), "{m:?}");
                assert_eq!(m.kept_apart_names, ["2026/06/28/DSC1.jpg"]);
                assert_eq!((after.rating, after.label.as_str()), (0, ""), "nothing put on the row");
                assert!(catalog.list_versions(row).unwrap().is_empty());
            }
        }
    }

    /// #249, review LOW-1 (probe P2): the bundle's identity is already held by a row of its
    /// own elsewhere in the library, while its original matches another row (OURS) by a proven
    /// capture. The bundle's data belongs to its own row: it goes there, and OURS gets none
    /// of it.
    #[test]
    fn a_proven_capture_match_never_takes_data_from_a_row_holding_the_bundles_identity() {
        use crate::scanner::same_photo::test_files::{exiftool_available, stamped_jpeg};
        if !exiftool_available("a_proven_capture_match_never_takes_data_from_a_row_holding_the_bundles_identity") {
            return;
        }
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const OURS: &str = "7a2d2f1f-3c8b-4d4e-8f90-1b2c3d4e5f60";
        let src = temp_dir("249-held-src").join("DSC1.jpg");
        stamped_jpeg(&src, "2026:06:28 12:00:00", "123", "4711");
        let (catalog, root) = temp_catalog("249-held");
        let lib = root.join("2026/06/28/DSC1.jpg");
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::copy(&src, &lib).unwrap();
        crate::xmp::write_identifier(&lib, OURS).unwrap();
        let len = std::fs::metadata(&lib).unwrap().len() as i64;
        let ours = catalog.upsert_photo_with_identity(&lib, None, 1, len, Some(OURS)).unwrap().id;
        let elsewhere = root.join("2025/01/01/theirs.jpg");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, b"their row's own file").unwrap();
        let theirs = catalog.upsert_photo_with_identity(&elsewhere, None, 1, 20, Some(THEIRS)).unwrap().id;

        let mut bp = plain_photo(THEIRS, "2026/06/28/DSC1.jpg");
        bp.rating = 4;
        let bundle_path = bundle_of("249-held", vec![(bp, Some(src))]);
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.skipped_duplicate, 1);
        let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!(result.merge.photos_matched_by_capture, 0, "{:?}", result.merge);
        assert_eq!(catalog.get_photo(ours).unwrap().rating, 0, "OURS gets none of it");
        assert_eq!(catalog.get_photo(theirs).unwrap().rating, 4, "the bundle's own row gets it");
        assert_eq!(crate::xmp::read_identifier(&lib).as_deref(), Some(OURS));
    }

    /// L-a of the second #246/#185 review: the library's copy of the capture is at a ` (n)`
    /// name, under a row of another identity, and the bundle's own path (the plain name) is
    /// free. The bundle's photo is kept apart there too — never inserted as a metadata-only
    /// row at the free path, describing a file that is not there (or, later, another photo's).
    #[test]
    fn a_photo_kept_apart_at_a_numbered_name_gets_no_row_at_its_free_path() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const OURS: &str = "7a2d2f1f-3c8b-4d4e-8f90-1b2c3d4e5f60";
        let (catalog, root) = temp_catalog("185-apart-n");
        let original = root.join("2026/06/28/DSC01234 (2).ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();
        crate::xmp::write_identifier(&original, OURS).unwrap();
        catalog.upsert_photo_with_identity(&original, None, 1, 14, Some(OURS)).unwrap();

        let bundle_path = make_test_bundle("185-apart-n", THEIRS, "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.skipped_duplicate, 1);
        let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!((result.merge.photos_kept_apart, result.merge.photos_added), (1, 0), "{:?}", result.merge);
        assert!(catalog.get_photo_by_uuid(THEIRS).is_err(), "no row for the bundle's photo");
        assert_eq!(photo_paths(&catalog), ["2026/06/28/DSC01234 (2).ARW"]);
        assert!(!root.join("2026/06/28/DSC01234.ARW").exists());
    }

    fn photo_paths(catalog: &Catalog) -> Vec<String> {
        let mut stmt = catalog.conn().prepare("SELECT path FROM photos ORDER BY path").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    }

    /// M-3 of the #246/#185 review: the library's photo has a row of its own identity but its
    /// sidecar is gone (identity debt), and the bundle brings the same bytes under another
    /// identity. The owner's sidecar never receives the bundle's identity — not while
    /// unpacking, not while indexing — the bundle's photo is kept apart and counted once, and
    /// the row is untouched.
    #[test]
    fn a_foreign_identity_never_lands_in_the_owners_sidecar() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        const OURS: &str = "7a2d2f1f-3c8b-4d4e-8f90-1b2c3d4e5f60";
        let (catalog, root) = temp_catalog("185-foreign");
        let original = root.join("2026/06/28/DSC01234.ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();
        let row = catalog.upsert_photo_with_identity(&original, None, 1, 14, Some(OURS)).unwrap();
        catalog.set_culling(row.id, Some(5), None, None).unwrap();
        let sidecar = root.join("2026/06/28/DSC01234.ARW.xmp");
        assert!(!sidecar.exists(), "the owner's sidecar is missing");

        let bundle_path = make_test_bundle("185-foreign", THEIRS, "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.skipped_duplicate, 1);
        assert!(!sidecar.exists(), "unpacking writes no sidecar for the owner's file");
        let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!((result.merge.photos_kept_apart, result.merge.photos_added), (1, 0), "{:?}", result.merge);

        assert_ne!(crate::xmp::read_identifier(&original).as_deref(), Some(THEIRS));
        assert!(!sidecar.exists(), "indexing writes no sidecar for the owner's file either");
        assert_eq!(catalog.count_photos(&Default::default()).unwrap(), 1);
        let after = catalog.get_photo(row.id).unwrap();
        assert_eq!((after.uuid.as_str(), after.rating), (OURS, 5));
        assert_eq!(catalog.get_iptc(row.id).unwrap(), Default::default());
        let conflicts: Vec<_> = catalog
            .list_pending_identity()
            .unwrap()
            .into_iter()
            .filter(|r| r.error.contains(THEIRS))
            .collect();
        assert!(conflicts.is_empty(), "no conflict naming the bundle's identity: {conflicts:?}");
    }

    /// The library file the bundle's photo is found at, with no row at all, takes the bundle's
    /// identity in its sidecar during indexing — the binding the unpack no longer does.
    #[test]
    fn a_found_file_with_no_row_is_bound_to_the_bundles_identity_when_indexed() {
        const THEIRS: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        let (catalog, root) = temp_catalog("185-unrowed");
        let original = root.join("2026/06/28/DSC01234.ARW");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"FAKE RAW BYTES").unwrap();

        let bundle_path = make_test_bundle("185-unrowed", THEIRS, "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.skipped_duplicate, 1);
        assert_eq!(crate::xmp::read_identifier(&original), None, "not bound while unpacking");
        index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!(crate::xmp::read_identifier(&original).as_deref(), Some(THEIRS));
        assert_eq!(catalog.get_photo_by_uuid(THEIRS).unwrap().path, "2026/06/28/DSC01234.ARW");
    }

    // --- names a catalog row holds (#247) -----------------------------------------------

    fn row_at(catalog: &Catalog, rel: &str) -> Option<(i64, String, i64)> {
        catalog
            .conn()
            .query_row("SELECT id, uuid, missing FROM photos WHERE path = ?1", [rel], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok()
    }

    /// #247 for a bundle: a row whose file is gone holds its name; the bundle's photo of
    /// another identity, at that relative path, goes to ` (2)` with a row of its own, and the
    /// old row keeps its name and identity.
    #[test]
    fn a_bundle_photo_of_another_identity_never_takes_the_name_of_a_row_whose_file_is_gone() {
        const OLD: &str = "44444444-4444-4444-8444-444444444444";
        const NEW: &str = "55555555-5555-4555-8555-555555555555";
        let (catalog, root) = temp_catalog("247-other");
        let name = root.join("2026/06/28/DSC01234.ARW");
        std::fs::create_dir_all(name.parent().unwrap()).unwrap();
        std::fs::write(&name, b"OLD RAW BYTES!").unwrap();
        let old_id = catalog.upsert_photo_with_identity(&name, None, 1, 14, Some(OLD)).unwrap().id;
        std::fs::remove_file(&name).unwrap();

        let bundle_path = make_test_bundle("247-other", NEW, "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!(partial.copied, 1);
        assert_eq!(extracted[0].dest, root.join("2026/06/28/DSC01234 (2).ARW"));
        assert!(!name.exists());
        index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
        assert_eq!(row_at(&catalog, "2026/06/28/DSC01234.ARW").map(|r| (r.0, r.1)), Some((old_id, OLD.to_string())));
        assert_eq!(catalog.get_photo_by_uuid(NEW).unwrap().path, "2026/06/28/DSC01234 (2).ARW");
    }

    /// L-f of the third #246 review for a bundle: the photo's own row lost its file (its
    /// sidecar left behind, or gone with it); importing the bundle again puts the original
    /// back at the row's name and re-links the row — no ` (2)`, no second row.
    #[test]
    fn a_bundle_photo_whose_row_lost_its_file_goes_back_to_its_name() {
        const UUID: &str = "33333333-3333-4333-8333-333333333333";
        for sidecar_left in [true, false] {
            let tag = format!("247-relink-{sidecar_left}");
            let (catalog, root) = temp_catalog(&tag);
            let bundle_path = make_test_bundle(&tag, UUID, "2026/06/28/DSC01234.ARW");
            let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
            let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
            index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
            let name = root.join("2026/06/28/DSC01234.ARW");
            let (id, _, _) = row_at(&catalog, "2026/06/28/DSC01234.ARW").unwrap();
            std::fs::remove_file(&name).unwrap();
            if !sidecar_left {
                std::fs::remove_file(crate::xmp::sidecar_path(&name)).unwrap();
            }
            catalog.reconcile_missing_for(&[id]).unwrap();

            let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
            let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
            assert_eq!(partial.copied, 1, "{sidecar_left}");
            assert_eq!(extracted[0].dest, name, "{sidecar_left}");
            let result = index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap();
            assert_eq!((result.restored, result.restored_trashed, result.merge.photos_added), (1, 0, 0), "{result:?}");
            assert_eq!(row_at(&catalog, "2026/06/28/DSC01234.ARW"), Some((id, UUID.to_string(), 0)), "{sidecar_left}");
            assert_eq!(photo_paths(&catalog), ["2026/06/28/DSC01234.ARW"], "{sidecar_left}");
            assert!(!root.join("2026/06/28/DSC01234 (2).ARW").exists(), "{sidecar_left}");
            assert_eq!(crate::xmp::read_identifier(&name).as_deref(), Some(UUID), "{sidecar_left}");
        }
    }

    /// #231 F5 for a bundle (review LOW-2): the photo's own row was offloaded — no local
    /// location row, a verified backup — so its original in the bundle is not unpacked back
    /// to this disk; it is counted as offloaded, and the merge still finds the row by
    /// identity. With its local location row still there (lost, not offloaded) it goes back.
    #[test]
    fn a_bundle_photo_offloaded_here_is_not_unpacked_back() {
        const UUID: &str = "44444444-4444-4444-8444-444444444444";
        for offloaded in [true, false] {
            let tag = format!("f5-bundle-{offloaded}");
            let (catalog, root) = temp_catalog(&tag);
            let bundle_path = make_test_bundle(&tag, UUID, "2026/06/28/DSC01234.ARW");
            let import = || {
                let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
                let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
                index_bundle(&catalog, &manifest, &extracted, &root, partial).unwrap()
            };
            import();
            let name = root.join("2026/06/28/DSC01234.ARW");
            let (id, _, _) = row_at(&catalog, "2026/06/28/DSC01234.ARW").unwrap();
            let nas = catalog.add_volume("NAS", &root.join("../nas-unmounted"), crate::catalog::VolumeKind::Backup).unwrap();
            catalog.add_location(id, nas, "2026/06/28/DSC01234.ARW", crate::catalog::LocationRole::Backup).unwrap();
            catalog
                .conn()
                .execute("UPDATE photo_locations SET verified_hash = 'abc' WHERE photo_id = ?1 AND volume_id = ?2", [id, nas])
                .unwrap();
            if offloaded {
                catalog
                    .conn()
                    .execute(
                        "DELETE FROM photo_locations WHERE photo_id = ?1
                            AND volume_id IN (SELECT id FROM volumes WHERE kind = 'local')",
                        [id],
                    )
                    .unwrap();
            }
            std::fs::remove_file(&name).unwrap();
            std::fs::remove_file(crate::xmp::sidecar_path(&name)).unwrap();

            let again = import();
            if offloaded {
                assert_eq!((again.offloaded, again.copied, again.restored), (1, 0, 0), "{again:?}");
                assert!(!name.exists(), "not unpacked back to this disk");
            } else {
                assert_eq!((again.offloaded, again.copied, again.restored), (0, 1, 1), "{again:?}");
                assert!(name.exists());
            }
            assert_eq!(photo_paths(&catalog), ["2026/06/28/DSC01234.ARW"], "{offloaded}: one row");
            assert_eq!(again.merge.photos_existing, 1, "{offloaded}: merged onto its row by identity");
        }
    }

    /// Review LOW-5 of #231 N-4: an original whose sidecar's name would be over 255 bytes is
    /// not unpacked — its identity could never be written beside it — and the result says
    /// why; the merge inserts no row for it at that path either (it has no original here and
    /// its path is free, so it is a metadata-only row, as for an original the bundle lacks).
    #[test]
    fn an_original_too_long_for_a_sidecar_is_not_unpacked() {
        let src_dir = temp_dir("long-src");
        let src = src_dir.join("long.ARW");
        std::fs::write(&src, b"FAKE RAW BYTES").unwrap();
        let long = format!("{}.ARW", "D".repeat(249));
        assert!(!crate::scanner::same_photo::sidecar_name_fits(Path::new(&long)));
        let bundle_path = bundle_of(
            "long",
            vec![(plain_photo("55555555-5555-4555-8555-555555555555", &format!("2026/06/28/{long}")), Some(src))],
        );
        let (catalog, root) = temp_catalog("long");
        let (manifest, mut archive) = open_bundle(&bundle_path).unwrap();
        let (extracted, partial) = extract_originals(&catalog, &manifest, &mut archive, &root, |_, _| {}).unwrap();
        assert_eq!((partial.name_too_long, partial.copied, partial.errors), (1, 0, 0), "{partial:?}");
        assert!(extracted.is_empty());
        assert!(!root.join("2026/06/28").join(&long).exists());
    }

    #[test]
    fn progress_called_for_each_photo() {
        let bundle_path =
            make_test_bundle("prog", "uuid-prog-1", "2026/06/28/DSC01234.ARW");
        let (manifest, mut archive) = open_bundle(&bundle_path).expect("open_bundle");

        let dest = temp_dir("prog-dest");
        let calls = std::sync::Mutex::new(Vec::<(usize, usize)>::new());
        extract_originals(&empty_catalog(&dest), &manifest, &mut archive, &dest, |done, total| {
            calls.lock().unwrap().push((done, total));
        })
        .expect("extract");

        let calls = calls.into_inner().unwrap();
        // 1 photo → 1 progress call.
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (1, 1));
    }
}
