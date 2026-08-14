//! Full-pipeline test against the user's real photo folders. Scans a folder,
//! then generates a thumbnail for one photo — exercising RAW embedded-preview
//! extraction via exiftool on actual Sony ARW files.
//!
//! Skips gracefully (passes) when the sample folder isn't present, so it's safe
//! in CI or on another machine.

use chairphoto_lib::catalog::{Catalog, PhotoQuery};
use chairphoto_lib::scanner::scan_folder;
use chairphoto_lib::thumbnails::{preview_bytes, thumbnail_bytes};
use std::path::{Path, PathBuf};

mod common;

/// Scanning real files should populate both metadata tiers: promoted columns
/// (camera model, etc.) and the generic key-value entries. Skips if absent.
#[test]
fn scan_populates_metadata() {
    let root = home();
    let sample = root.join("Pictures/Raw");
    if !sample.is_dir() {
        eprintln!(
            "SKIPPED: scan_populates_metadata — {} not present, so no real file was scanned",
            sample.display()
        );
        return;
    }
    let tmp = common::TestTmpDir::new("metadata-test");
    let catalog = Catalog::open(&tmp.join("t.chairphoto"), &root).unwrap();
    scan_folder(&catalog, &sample, &chairphoto_lib::scanner::never_abort(), &|_| {}).unwrap();

    let photos = catalog.list_photos(&PhotoQuery::default()).unwrap();
    // Find an ARW (Sony) and check its promoted + generic metadata.
    let arw = photos
        .iter()
        .find(|p| p.path.to_lowercase().ends_with(".arw"))
        .expect("expected an ARW photo");

    let full = catalog.get_photo(arw.id).unwrap();
    assert!(full.camera_model.is_some(), "camera_model should be promoted");
    eprintln!(
        "camera_model={:?} lens={:?}",
        full.camera_model, full.lens
    );

    let entries = catalog.get_photo_metadata(arw.id).unwrap();
    assert!(entries.len() > 20, "expected many generic metadata entries");
    assert!(
        entries.iter().any(|e| e.group_name == "EXIF"),
        "expected EXIF group entries"
    );
    // Binary blobs must be excluded.
    assert!(
        !entries.iter().any(|e| e.value.starts_with("(Binary data")),
        "binary blobs should be filtered out"
    );
    eprintln!("{} generic metadata entries stored", entries.len());
}

/// Verify the v1 -> v2 upgrade on a COPY of the real catalog: every existing photo
/// should get a backfilled primary location and resolve to a path. Non-destructive
/// (operates on a copy); skips if the real catalog isn't present.
#[test]
fn migrates_real_v1_catalog_and_backfills_locations() {
    let home = home();
    let real = home.join(".local/share/chairphoto/default.chairphoto");
    if !real.is_file() {
        eprintln!(
            "SKIPPED: migrates_real_v1_catalog_and_backfills_locations — no real catalog at {}, \
             so the v1→v2 migration was never exercised",
            real.display()
        );
        return;
    }
    let tmp = common::TestTmpDir::new("migrate-test");
    let copy = tmp.join("copy.chairphoto");
    std::fs::copy(&real, &copy).unwrap();

    // Opening runs migration. Root is $HOME (what the catalog stored).
    let catalog = Catalog::open(&copy, &home).unwrap();

    let volumes = catalog.list_volumes().unwrap();
    assert!(!volumes.is_empty(), "migration should create the default volume");

    let photos = catalog.list_photos(&PhotoQuery::default()).unwrap();
    eprintln!("migrated catalog has {} photos", photos.len());
    for photo in photos.iter().take(5) {
        let locations = catalog.photo_locations(photo.id).unwrap();
        assert!(
            !locations.is_empty(),
            "photo {} should have a backfilled location",
            photo.path
        );
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

#[test]
fn scans_real_folder_and_extracts_raw_thumbnail() {
    let root = home();
    let sample = root.join("Pictures/Raw");
    if !sample.is_dir() {
        eprintln!(
            "SKIPPED: scans_real_folder_and_extracts_raw_thumbnail — {} not present, so no RAW \
             embedded-preview extraction was exercised",
            sample.display()
        );
        return;
    }

    let tmp = common::TestTmpDir::new("pipeline-test");
    let catalog = Catalog::open(&tmp.join("t.chairphoto"), &root).unwrap();

    let result = scan_folder(&catalog, &sample, &chairphoto_lib::scanner::never_abort(), &|_| {}).unwrap();
    eprintln!(
        "scanned={} imported={} created={} errors={}",
        result.scanned, result.imported, result.created, result.errors
    );
    assert!(result.imported > 0, "expected to import at least one photo");

    // Exercise one file of each extension present (e.g. ARW and DNG) — they store
    // previews differently (Sony embeds JPEG, Adobe DNG embeds TIFF).
    let photos = catalog.list_photos(&PhotoQuery::default()).unwrap();
    let mut tested_extensions = std::collections::HashSet::new();
    for photo in &photos {
        let ext = Path::new(&photo.path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !tested_extensions.insert(ext.clone()) {
            continue; // already covered this extension
        }
        let absolute = catalog.to_absolute(&photo.path);
        assert!(Path::new(&absolute).exists());

        let thumb = thumbnail_bytes(&absolute)
            .unwrap_or_else(|e| panic!("thumbnail failed for {}: {e}", photo.path));
        assert_eq!(&thumb[0..2], &[0xff, 0xd8], "thumbnail not a JPEG for {}", photo.path);

        let preview = preview_bytes(&absolute)
            .unwrap_or_else(|e| panic!("preview failed for {}: {e}", photo.path));
        assert_eq!(&preview[0..2], &[0xff, 0xd8], "preview not a JPEG for {}", photo.path);

        eprintln!(
            ".{ext}: {} -> thumbnail={} bytes, preview={} bytes",
            photo.path,
            thumb.len(),
            preview.len()
        );
    }
}
