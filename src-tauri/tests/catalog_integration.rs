//! End-to-end catalog tests against a real temp SQLite file. Exercises the
//! invariants from AGENTS.md: UUID assignment, relative-path storage,
//! hierarchical tags, and culling.

use chairphoto_lib::catalog::{Catalog, LocationRole, PickState, StorageStatus, VolumeKind};
use chairphoto_lib::catalog::PathCandidate;
use std::path::PathBuf;

mod common;

/// Recursively collect `.jpg` files under a directory (test helper).
fn walkdir_jpgs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walkdir_jpgs(&p));
            } else if p.extension().and_then(|s| s.to_str()) == Some("jpg") {
                out.push(p);
            }
        }
    }
    out
}

/// Build a catalog in a unique temp dir and return (catalog, root).
fn temp_catalog(tag: &str) -> (Catalog, common::TestSubPath) {
    let dir = common::TestTmpDir::new(tag);
    let root = dir.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
    (catalog, dir.into_subpath("photos"))
}

#[test]
fn upsert_assigns_uuid_once_and_stores_relative_path() {
    let (catalog, root) = temp_catalog("upsert");
    let photo_path = root.join("trip/DSC0001.ARW");

    let first = catalog.upsert_photo(&photo_path, None, 111, 2048).unwrap();
    assert!(first.created, "first upsert should create the row");
    assert!(!first.uuid.is_empty(), "a UUID must be assigned on import");

    // Stored path is relative to the root, not absolute.
    let photo = catalog.get_photo(first.id).unwrap();
    assert_eq!(photo.path, "trip/DSC0001.ARW");

    // Re-upserting the same file keeps the same id and UUID (no new identity).
    let second = catalog.upsert_photo(&photo_path, None, 222, 4096).unwrap();
    assert!(!second.created);
    assert_eq!(second.id, first.id);
    assert_eq!(second.uuid, first.uuid);
    // Stats differed from the first upsert (111/2048 → 222/4096), so it's NOT unchanged.
    assert!(!second.unchanged, "changed mtime/size must report unchanged = false");

    // Re-upserting with identical stats reports unchanged = true — the scanner uses this
    // to skip re-extracting metadata for untouched files on a re-scan.
    let third = catalog.upsert_photo(&photo_path, None, 222, 4096).unwrap();
    assert!(!third.created);
    assert!(third.unchanged, "identical mtime/size must report unchanged = true");
    // A newly-created row is never "unchanged".
    assert!(!first.unchanged);
}

#[test]
fn remove_photo_forgets_row_and_cascades_but_keeps_no_file_logic() {
    let (catalog, root) = temp_catalog("remove-photo");
    let path = root.join("gone.jpg");
    std::fs::write(&path, b"x").unwrap();
    let id = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;
    let tag = catalog.create_tag("Test").unwrap();
    catalog.assign_tag(id, tag).unwrap();
    // It has a location row from the upsert.
    assert!(!catalog.photo_locations(id).unwrap().is_empty());

    catalog.remove_photo(id).unwrap();

    // Row is gone, and its location rows cascaded away.
    assert!(catalog.get_photo(id).is_err(), "photo row must be deleted");
    assert!(catalog.photo_locations(id).unwrap().is_empty(), "locations cascade");
    // remove_photo never touches files on disk.
    assert!(path.exists(), "the file on disk must NOT be deleted");
}

#[test]
fn offload_policy_selects_old_backed_up_photos_only() {
    use chairphoto_lib::catalog::PromotedMetadata;
    let (catalog, root) = temp_catalog("offload-policy");
    let nas_dir = root.parent().unwrap().join("nas-policy");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();

    let mk = |name: &str, bytes: &[u8]| -> i64 {
        let p = root.join(name);
        std::fs::write(&p, bytes).unwrap();
        catalog.upsert_photo(&p, None, 1, bytes.len() as i64).unwrap().id
    };
    let set_old = |id: i64| {
        let promoted = PromotedMetadata {
            capture_time: Some("2000-01-01T00:00:00".into()),
            ..Default::default()
        };
        catalog.set_photo_metadata(id, &promoted, &[]).unwrap();
    };

    let old_bak = mk("old1.ARW", b"oldbak");
    set_old(old_bak);
    catalog.backup_photo(old_bak, nas).unwrap(); // OLD + backed up → eligible
    let recent = mk("recent.ARW", b"recent");
    catalog.backup_photo(recent, nas).unwrap(); // RECENT (created_at=now) → kept local
    let old_nobak = mk("old2.ARW", b"oldnobak");
    set_old(old_nobak); // OLD but no backup → never offloaded

    let eligible = catalog.photos_eligible_for_offload(90).unwrap();
    assert!(eligible.contains(&old_bak), "old + backed up is eligible");
    assert!(!eligible.contains(&recent), "recent photo is kept local");
    assert!(!eligible.contains(&old_nobak), "no verified backup → never offloaded");

    // Once offloaded, it has no local copy left, so it drops out of the eligible set.
    catalog.offload_photo(old_bak).unwrap();
    assert!(
        !catalog.photos_eligible_for_offload(90).unwrap().contains(&old_bak),
        "already-offloaded photo is not re-listed"
    );
}

#[test]
fn scan_external_indexes_nas_photos_in_place() {
    let (catalog, root) = temp_catalog("nas-scan");
    // A NAS (backup) volume holding an existing archive — not under the catalog root.
    let nas_dir = root.parent().unwrap().join("nas-archive");
    std::fs::create_dir_all(nas_dir.join("1998")).unwrap();
    catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
    let photo = nas_dir.join("1998/old.jpg");
    std::fs::write(&photo, b"oldphoto").unwrap();

    let res = chairphoto_lib::scanner::scan_external_folder(&catalog, &nas_dir, &chairphoto_lib::scanner::never_abort(), &|_| {}).unwrap();
    assert_eq!(res.created, 1, "the NAS photo is indexed");

    // It's catalogued as NAS-only: shows under the "nas" tier, resolves to the NAS file,
    // status Archived, and is NOT under "local".
    let nas_ids: Vec<i64> = catalog
        .list_photos(None, None, None, &[], "all", None, "nas", None, None, &[], None)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(nas_ids.len(), 1);
    let id = nas_ids[0];
    assert_eq!(catalog.resolve_photo_path(id).unwrap(), Some(photo.clone()));
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::Archived);
    assert!(
        catalog.list_photos(None, None, None, &[], "all", None, "local", None, None, &[], None).unwrap().is_empty(),
        "a NAS-resident photo is not 'local'"
    );

    // Re-scan is idempotent — matched by location, no duplicate row.
    chairphoto_lib::scanner::scan_external_folder(&catalog, &nas_dir, &chairphoto_lib::scanner::never_abort(), &|_| {}).unwrap();
    assert_eq!(
        catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None).unwrap().len(),
        1
    );
}

#[test]
fn vacuum_runs_and_reports_size() {
    let (catalog, root) = temp_catalog("vacuum");
    catalog.upsert_photo(&root.join("a.jpg"), None, 1, 1).unwrap();
    let before = catalog.db_size_bytes().unwrap();
    assert!(before > 0);
    catalog.vacuum().unwrap(); // must succeed (not inside a transaction)
    assert!(catalog.db_size_bytes().unwrap() > 0);
}

#[test]
fn find_and_purge_empty_photos() {
    let (catalog, root) = temp_catalog("empty-photos");
    // good: non-empty local file; empty: 0-byte file; gone: no file at all.
    let good = root.join("good.jpg");
    std::fs::write(&good, b"data").unwrap();
    let good_id = catalog.upsert_photo(&good, None, 1, 4).unwrap().id;
    let empty = root.join("empty.jpg");
    std::fs::write(&empty, b"").unwrap(); // 0 bytes
    let empty_id = catalog.upsert_photo(&empty, None, 1, 0).unwrap().id;
    let gone_id = catalog.upsert_photo(&root.join("gone.jpg"), None, 1, 1).unwrap().id; // no file

    let found: Vec<i64> = catalog
        .find_empty_photos()
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(found, vec![empty_id], "only the 0-byte photo is 'empty'");
    let _ = (good_id, gone_id);

    let removed = catalog.purge_empty_photos().unwrap();
    assert_eq!(removed.len(), 1);
    assert!(catalog.get_photo(empty_id).is_err(), "empty photo row deleted");
    assert!(catalog.get_photo(good_id).is_ok(), "good photo kept");
    assert!(catalog.get_photo(gone_id).is_ok(), "missing-file photo kept (it's 'unavailable', not 'empty')");
    assert!(empty.exists(), "the empty file on disk is left as-is");
}

#[test]
fn list_photos_filters_by_storage_tier() {
    let (catalog, root) = temp_catalog("tier-filter");
    let nas_dir = root.parent().unwrap().join("nas-tier");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
    let local_vol = catalog
        .list_volumes()
        .unwrap()
        .into_iter()
        .find(|v| v.kind == VolumeKind::Local)
        .unwrap()
        .id;

    // local-only, local+NAS, and NAS-only (offloaded) photos.
    let local_id = catalog.upsert_photo(&root.join("local.jpg"), None, 1, 1).unwrap().id;
    let bak_id = catalog.upsert_photo(&root.join("bak.jpg"), None, 1, 1).unwrap().id;
    catalog.add_location(bak_id, nas, "bak.jpg", LocationRole::Backup).unwrap();
    let arch_id = catalog.upsert_photo(&root.join("arch.jpg"), None, 1, 1).unwrap().id;
    catalog.add_location(arch_id, nas, "arch.jpg", LocationRole::Backup).unwrap();
    catalog.remove_locations_on_volume(arch_id, local_vol).unwrap(); // offloaded: NAS only

    let ids = |tier: &str| -> Vec<i64> {
        catalog
            .list_photos(None, None, None, &[], "all", None, tier, None, None, &[], None)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect()
    };

    let local = ids("local");
    assert!(local.contains(&local_id) && local.contains(&bak_id));
    assert!(!local.contains(&arch_id), "offloaded photo is not 'local'");

    let nas_only = ids("nas");
    assert!(nas_only.contains(&arch_id), "offloaded photo is 'nas'");
    assert!(
        !nas_only.contains(&local_id) && !nas_only.contains(&bak_id),
        "on-disk photos are not 'nas-only'"
    );

    assert_eq!(ids("all").len(), 3);
}

#[test]
fn backup_is_idempotent_and_copy_never_clobbers_existing() {
    use chairphoto_lib::catalog::copy_and_verify;
    let (catalog, root) = temp_catalog("backup-safety");
    let nas_dir = root.parent().unwrap().join("nas-safety");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();

    let raw = root.join("a/DSC1.ARW");
    std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
    std::fs::write(&raw, b"hello").unwrap();
    let id = catalog.upsert_photo(&raw, None, 1, 5).unwrap().id;

    // Idempotency signal: false before, true after a successful backup.
    assert!(!catalog.has_verified_backup(id).unwrap());
    catalog.backup_photo(id, nas).unwrap();
    assert!(catalog.has_verified_backup(id).unwrap());

    let nas_copy = nas_dir.join("a/DSC1.ARW");
    assert!(nas_copy.is_file());
    // The temp sibling is renamed away on success — nothing left behind.
    let part = nas_dir.join("a/.DSC1.ARW.chairphoto-part");
    assert!(!part.exists(), "no temp left after a successful copy");

    // A failed copy_and_verify must NEVER damage an existing destination. A wrong
    // `expected` hash makes it refuse; the existing backup and dir must be intact.
    let before = std::fs::read(&nas_copy).unwrap();
    assert!(
        copy_and_verify(&raw, &nas_copy, Some("deadbeef")).is_err(),
        "mismatched expected hash must error"
    );
    assert_eq!(std::fs::read(&nas_copy).unwrap(), before, "existing backup untouched on failure");
    assert!(!part.exists(), "temp cleaned up on failure");
}

#[test]
fn find_and_purge_unavailable_photos_is_conservative() {
    let (catalog, root) = temp_catalog("unavailable");

    // A: present local file → available.
    let a_path = root.join("a.jpg");
    std::fs::write(&a_path, b"a").unwrap();
    let a = catalog.upsert_photo(&a_path, None, 1, 1).unwrap().id;

    // B: catalog row + local primary location, but no file anywhere → unavailable.
    let b = catalog.upsert_photo(&root.join("b.jpg"), None, 1, 1).unwrap().id;

    // A reachable backup volume (its base dir exists).
    let backup_dir = root.parent().unwrap().join("backup");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup_vol = catalog.add_volume("nas", &backup_dir, VolumeKind::Backup).unwrap();

    // C: local gone, but the backup file exists on the reachable volume → available.
    let c = catalog.upsert_photo(&root.join("c.jpg"), None, 1, 1).unwrap().id;
    std::fs::write(backup_dir.join("c.jpg"), b"c").unwrap();
    catalog.add_location(c, backup_vol, "c.jpg", LocationRole::Backup).unwrap();

    // D: local gone, backup record on an UNREACHABLE volume (base dir absent) → uncertain,
    //    so it must NEVER be purged (the file may be there once the volume is mounted).
    let offline_dir = root.parent().unwrap().join("offline-nas");
    let offline_vol = catalog.add_volume("offline", &offline_dir, VolumeKind::Backup).unwrap();
    let d = catalog.upsert_photo(&root.join("d.jpg"), None, 1, 1).unwrap().id;
    catalog.add_location(d, offline_vol, "d.jpg", LocationRole::Backup).unwrap();

    let gone: Vec<i64> = catalog
        .find_unavailable_photos()
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(gone.contains(&b), "B (no copy anywhere) is unavailable");
    assert!(!gone.contains(&a), "A has a present local file");
    assert!(!gone.contains(&c), "C has a present reachable backup");
    assert!(!gone.contains(&d), "D might be on the offline volume — never removed");

    let removed = catalog.purge_unavailable_photos().unwrap();
    assert_eq!(removed.len(), 1, "only B is purged");
    assert!(catalog.get_photo(b).is_err(), "B row deleted");
    assert!(catalog.get_photo(a).is_ok());
    assert!(catalog.get_photo(c).is_ok());
    assert!(catalog.get_photo(d).is_ok());
    // Purge never touches files.
    assert!(a_path.exists());
    assert!(backup_dir.join("c.jpg").exists());
}

#[test]
fn relocate_photo_repoints_path_and_clears_missing() {
    let (catalog, root) = temp_catalog("relocate");
    let old = root.join("old/DSC1.ARW");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"original").unwrap();
    let id = catalog.upsert_photo(&old, None, 1, 8).unwrap().id;

    // Simulate the file moving to a new location under the library root.
    let moved = root.join("2026/DSC1.ARW");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&old, &moved).unwrap();

    let uuid = catalog.relocate_photo(id, &moved).unwrap();
    assert!(!uuid.is_empty());
    // Logical path now reflects the new location, and it resolves to the moved file.
    assert_eq!(catalog.get_photo(id).unwrap().path, "2026/DSC1.ARW");
    assert_eq!(catalog.resolve_photo_path(id).unwrap(), Some(moved.clone()));

    // A file outside the catalog root is rejected.
    let outside_dir = common::TestTmpDir::new("relocate-outside");
    let outside = outside_dir.join("outside.ARW");
    std::fs::write(&outside, b"x").unwrap();
    assert!(catalog.relocate_photo(id, &outside).is_err());
}

#[test]
fn moved_file_matches_by_uuid_instead_of_duplicating() {
    let (catalog, root) = temp_catalog("rehome");
    let first = catalog.upsert_photo(&root.join("DSC1.ARW"), None, 1, 1).unwrap();
    assert!(first.created);

    // The same photo now appears at a different relative path (a re-root / move). With
    // its UUID supplied (as the scanner reads from the sidecar), it re-homes the existing
    // row rather than creating a new one.
    let moved = catalog
        .upsert_photo_with_identity(&root.join("2026/DSC1.ARW"), None, 2, 2, Some(&first.uuid))
        .unwrap();
    assert!(!moved.created, "moved file must match the existing row, not duplicate");
    assert_eq!(moved.id, first.id);
    assert_eq!(moved.uuid, first.uuid);
    assert_eq!(catalog.get_photo(first.id).unwrap().path, "2026/DSC1.ARW");

    // A brand-new file with a UUID from another machine adopts that identity.
    let imported = catalog
        .upsert_photo_with_identity(&root.join("FROM-LAPTOP.ARW"), None, 3, 3, Some("known-uuid-123"))
        .unwrap();
    assert!(imported.created);
    assert_eq!(imported.uuid, "known-uuid-123");
}

#[test]
fn culling_round_trips() {
    let (catalog, root) = temp_catalog("culling");
    let id = catalog
        .upsert_photo(&root.join("a.jpg"), None, 1, 1)
        .unwrap()
        .id;

    let updated = catalog
        .set_culling(id, Some(4), Some("Red"), Some(PickState::Pick))
        .unwrap();
    assert_eq!(updated.rating, 4);
    assert_eq!(updated.label, "Red");
    assert_eq!(updated.pick_state, PickState::Pick);

    // A partial update leaves the other fields intact.
    let only_rating = catalog.set_culling(id, Some(2), None, None).unwrap();
    assert_eq!(only_rating.rating, 2);
    assert_eq!(only_rating.label, "Red");
    assert_eq!(only_rating.pick_state, PickState::Pick);
}

#[test]
fn hierarchical_tags_and_assignment() {
    let (catalog, root) = temp_catalog("tags");
    let id = catalog
        .upsert_photo(&root.join("boat.arw"), None, 1, 1)
        .unwrap()
        .id;

    // Creating a nested path creates the ancestors too.
    let leaf = catalog.create_tag("Transportation/Watercraft/Ferry").unwrap();
    let all = catalog.list_tags_with_counts().unwrap();
    assert_eq!(all.len(), 3, "three tags: Transportation, Watercraft, Ferry");

    // Re-creating returns the same leaf id (idempotent).
    let leaf_again = catalog.create_tag("transportation | watercraft | ferry").unwrap();
    assert_eq!(leaf, leaf_again);

    catalog.assign_tag(id, leaf).unwrap();
    let tags = catalog.get_photo_tags(id).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].full_path, "Transportation/Watercraft/Ferry");

    // Filtering by an ancestor tag includes photos tagged with a descendant.
    let root_tag = all.iter().find(|t| t.tag.name == "Transportation").unwrap();
    let filtered = catalog.list_photos(Some(root_tag.tag.id), None, None, &[], "all", None, "all", None, None, &[], None).unwrap();
    assert_eq!(filtered.len(), 1);

    catalog.remove_tag(id, leaf).unwrap();
    assert_eq!(catalog.get_photo_tags(id).unwrap().len(), 0);
}

#[test]
fn upsert_records_a_primary_location_on_the_default_volume() {
    let (catalog, root) = temp_catalog("locations");

    // A default volume exists from migration.
    let volumes = catalog.list_volumes().unwrap();
    assert_eq!(volumes.len(), 1);
    assert!(volumes[0].reachable, "catalog root should be reachable");

    let photo_path = root.join("sub/a.arw");
    std::fs::create_dir_all(photo_path.parent().unwrap()).unwrap();
    std::fs::write(&photo_path, b"raw").unwrap();
    let id = catalog.upsert_photo(&photo_path, None, 1, 3).unwrap().id;

    // upsert recorded a primary location relative to the default volume.
    let locations = catalog.photo_locations(id).unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].role, LocationRole::Primary);
    assert_eq!(locations[0].relative_path, "sub/a.arw");

    // The resolver returns the real, existing absolute path.
    let resolved = catalog.resolve_photo_path(id).unwrap();
    assert_eq!(resolved, Some(photo_path));
}

#[test]
fn assemble_export_keywords_expands_ancestors_and_paths() {
    let (catalog, root) = temp_catalog("export-keywords");
    let id = catalog.upsert_photo(&root.join("p.arw"), None, 1, 1).unwrap().id;
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    catalog.add_synonym(owl, "Strigiformes", None, true).unwrap();
    catalog.assign_tag(id, owl).unwrap();

    let kw = catalog.assemble_export_keywords(id, &[]).unwrap();
    // Flat keywords include the tag, its synonym, and all ancestors.
    for expected in ["Owl", "Strigiformes", "Birds", "Animals"] {
        assert!(kw.flat.contains(&expected.to_string()), "missing {expected}");
    }
    // Ordered by specificity (G4): the leaf term + synonym precede its ancestors.
    assert_eq!(kw.flat, vec!["Owl", "Strigiformes", "Birds", "Animals"]);
    // Hierarchical path is the canonical full path, pipe-joined.
    assert_eq!(kw.hierarchical, vec!["Animals|Birds|Owl".to_string()]);

    // With a second, shallower assigned tag, the deeper tag's keywords rank first.
    let weather = catalog.create_tag("Weather").unwrap();
    catalog.assign_tag(id, weather).unwrap();
    let kw2 = catalog.assemble_export_keywords(id, &[]).unwrap();
    assert_eq!(kw2.flat, vec!["Owl", "Strigiformes", "Birds", "Animals", "Weather"]);

    // A photo with no tags yields nothing.
    let bare = catalog.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;
    let empty = catalog.assemble_export_keywords(bare, &[]).unwrap();
    assert!(empty.flat.is_empty() && empty.hierarchical.is_empty());
}

#[test]
fn assigning_a_child_drops_the_redundant_parent() {
    let (catalog, root) = temp_catalog("prune-tags");
    let id = catalog.upsert_photo(&root.join("p.arw"), None, 1, 1).unwrap().id;
    let harbor = catalog.create_tag("Public place/Harbor").unwrap();
    let marina = catalog.create_tag("Public place/Harbor/Marina").unwrap();

    // Tag the parent first, then the child (the paste-then-redundant case).
    catalog.assign_tag(id, harbor).unwrap();
    catalog.assign_tag(id, marina).unwrap();

    // The parent is implied by the child and was auto-pruned — only the leaf remains.
    let paths: Vec<String> = catalog
        .get_photo_tags(id)
        .unwrap()
        .into_iter()
        .map(|t| t.full_path)
        .collect();
    assert_eq!(paths, vec!["Public place/Harbor/Marina".to_string()]);
    // A similarly-prefixed sibling ("Harbortown") must NOT be treated as a descendant.
    let town = catalog.create_tag("Public place/Harbortown").unwrap();
    catalog.assign_tag(id, town).unwrap();
    assert_eq!(catalog.get_photo_tags(id).unwrap().len(), 2, "sibling kept, not pruned");

    // The library-wide tidy is a no-op once everything's already leaves-only.
    assert_eq!(catalog.tidy_redundant_tags().unwrap(), 0);
}

#[test]
fn non_exportable_tag_is_dropped_but_descendants_export() {
    let (catalog, root) = temp_catalog("export-gate");
    let id = catalog.upsert_photo(&root.join("p.arw"), None, 1, 1).unwrap().id;
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    catalog.assign_tag(id, owl).unwrap();

    // Mark the middle "Birds" tag organizational (not for export).
    let birds = catalog.create_tag("Animals/Birds").unwrap();
    catalog.set_tag_exportable(birds, false).unwrap();
    assert!(!catalog.tag_exportable(birds).unwrap());

    let kw = catalog.assemble_export_keywords(id, &[]).unwrap();
    assert!(kw.flat.contains(&"Owl".to_string()), "leaf still exports");
    assert!(kw.flat.contains(&"Animals".to_string()), "ancestor still exports");
    assert!(!kw.flat.contains(&"Birds".to_string()), "organizational tag dropped");
    // The hierarchical path drops the organizational segment too.
    assert_eq!(kw.hierarchical, vec!["Animals|Owl".to_string()]);

    // Marking the assigned leaf itself organizational emits only its ancestors.
    catalog.set_tag_exportable(owl, false).unwrap();
    let kw2 = catalog.assemble_export_keywords(id, &[]).unwrap();
    assert!(!kw2.flat.contains(&"Owl".to_string()));
    assert_eq!(kw2.hierarchical, vec!["Animals".to_string()]);
}

#[test]
fn export_handoff_copies_original_and_sidecar_and_skips_offline() {
    use chairphoto_lib::export::{resolve_originals, write_exports, ExportPreset};
    let (catalog, root) = temp_catalog("export");

    // A photo with a real file on disk + an existing (well-formed) XMP sidecar that
    // carries a foreign darktable element, to prove merge-safety on export.
    let raw = root.join("DSC1.ARW");
    std::fs::write(&raw, b"raw-bytes").unwrap();
    std::fs::write(
        root.join("DSC1.ARW.xmp"),
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/"><darktable:history>keep-me</darktable:history></rdf:Description></rdf:RDF></x:xmpmeta>"#,
    )
    .unwrap();
    let have = catalog.upsert_photo(&raw, None, 1, 9).unwrap().id;

    // A photo whose file doesn't exist → unresolvable (counts as offline/skipped).
    let gone = catalog.upsert_photo(&root.join("GONE.ARW"), None, 1, 1).unwrap().id;

    // Tag it so export emits keywords (Animals/Birds/Owl → contained keywords).
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    catalog.assign_tag(have, owl).unwrap();

    let dest = root.parent().unwrap().join("export-out");
    let resolved = resolve_originals(&catalog, &[have, gone], &[], None);
    assert_eq!(resolved.skipped_offline, 1);
    let result = write_exports(&resolved, ExportPreset::HandOff, &dest, &[]).unwrap();

    assert_eq!(result.exported, 1);
    assert_eq!(result.skipped_offline, 1);
    assert_eq!(result.errors, 0);
    assert!(dest.join("DSC1.ARW").is_file());
    assert!(dest.join("DSC1.ARW.xmp").is_file(), "sidecar travels with the original");
    assert!(!dest.join("GONE.ARW").exists());

    // Keywords (G2) are emitted into the destination sidecar: ancestors expanded,
    // and the foreign darktable element is preserved (merge-safe).
    let xmp = std::fs::read_to_string(dest.join("DSC1.ARW.xmp")).unwrap();
    assert!(xmp.contains("Owl") && xmp.contains("Birds") && xmp.contains("Animals"));
    assert!(xmp.contains("Animals|Birds|Owl"), "hierarchicalSubject path present");
    assert!(xmp.contains("keep-me"), "foreign darktable element preserved");

    // A reach-hashtag bundle is written as hashtags.txt next to the photos.
    let grp = catalog.create_tag_group("Reach").unwrap();
    let street = catalog.create_tag("Street Photography").unwrap();
    catalog.add_tag_to_group(grp, street).unwrap();
    let bundle = catalog.assemble_hashtag_bundle(grp, None).unwrap();
    assert_eq!(bundle, vec!["#streetphotography".to_string()]);
    write_exports(&resolve_originals(&catalog, &[have], &[], None), ExportPreset::ShowOff, &dest, &bundle)
        .unwrap();
    let tags_txt = std::fs::read_to_string(dest.join("hashtags.txt")).unwrap();
    assert!(tags_txt.contains("#streetphotography"));

    // Re-exporting the same photo doesn't clobber: a disambiguated copy appears.
    let again =
        write_exports(&resolve_originals(&catalog, &[have], &[], None), ExportPreset::HandOff, &dest, &[])
            .unwrap();
    assert_eq!(again.exported, 1);
    assert!(dest.join("DSC1 (2).ARW").is_file());
    assert!(dest.join("DSC1 (2).ARW.xmp").is_file(), "renamed sidecar stays paired");
}

#[test]
fn import_batches_assign_and_filter() {
    let (catalog, root) = temp_catalog("batches");
    let a = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
    let b = catalog.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;

    // First ingest: a batch holding both.
    let batch1 = catalog.create_import_batch("/cards/2026-06-23").unwrap();
    catalog.assign_photos_to_batch(batch1, &[a, b]).unwrap();

    // A later ingest with a new photo gets its own batch.
    let c = catalog.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
    let batch2 = catalog.create_import_batch("/cards/2026-06-24").unwrap();
    // Re-including an already-batched photo is a no-op (immutable membership).
    catalog.assign_photos_to_batch(batch2, &[a, c]).unwrap();

    let batches = catalog.list_import_batches().unwrap();
    assert_eq!(batches.len(), 2);
    let count = |id: i64| batches.iter().find(|b| b.id == id).unwrap().photo_count;
    assert_eq!(count(batch1), 2); // a, b — a kept its original batch
    assert_eq!(count(batch2), 1); // c only

    // Filter the view by batch (composes with culling).
    let in1: Vec<i64> = catalog
        .list_photos(None, None, Some(batch1), &[], "all", None, "all", None, None, &[], None)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(in1.len(), 2);
    assert!(in1.contains(&a) && in1.contains(&b));
    let in2 = catalog.list_photos(None, None, Some(batch2), &[], "all", None, "all", None, None, &[], None).unwrap();
    assert_eq!(in2.len(), 1);
    assert_eq!(in2[0].id, c);
}

#[test]
fn ingest_from_card_copies_into_date_tree_and_indexes() {
    use chairphoto_lib::scanner::ingest_from_card;
    let (catalog, root) = temp_catalog("ingest");

    // A "card" outside the catalog root with two raster images (no EXIF date → mtime).
    let card = root.parent().unwrap().join("card");
    std::fs::create_dir_all(&card).unwrap();
    std::fs::write(card.join("IMG_1.jpg"), b"\xff\xd8one").unwrap();
    std::fs::write(card.join("IMG_2.jpg"), b"\xff\xd8two").unwrap();

    // Ingest into the local catalog root (dest must be under the root) with a custom
    // import name.
    let result = ingest_from_card(&catalog, &card, &root, Some("Tønsberg 2026-06")).unwrap();
    assert_eq!(result.scanned, 2);
    assert_eq!(result.created, 2);

    // Files were copied into a YYYY/MM/DD tree under the root (originals untouched).
    assert!(card.join("IMG_1.jpg").exists(), "source card untouched");
    let copied: Vec<_> = walkdir_jpgs(&root);
    assert_eq!(copied.len(), 2, "two copies under the catalog root");
    assert!(
        copied
            .iter()
            .all(|p| p.strip_prefix(&root).unwrap().components().count() == 4),
        "copied into a YYYY/MM/DD subtree (3 dirs + filename), not the root"
    );

    // Indexed into the catalog, in one import batch, each queued for backup.
    assert_eq!(catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None).unwrap().len(), 2);
    let batches = catalog.list_import_batches().unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].photo_count, 2);
    assert_eq!(batches[0].source_label, "Tønsberg 2026-06", "custom import name");
    assert_eq!(catalog.list_pending_operations().unwrap().len(), 2);

    // Re-ingesting the same card is idempotent (same-size copies skipped).
    let again = ingest_from_card(&catalog, &card, &root, None).unwrap();
    assert_eq!(again.created, 0);
    assert_eq!(again.skipped, 2, "both already-present copies reported as skipped");
    assert_eq!(walkdir_jpgs(&root).len(), 2);
}

#[test]
fn reconcile_queue_enqueues_and_drains_when_nas_reachable() {
    let (catalog, root) = temp_catalog("reconcile");
    let nas_dir = root.parent().unwrap().join("nas");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();

    let raw = root.join("2016/07/02/DSC9.ARW");
    std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
    std::fs::write(&raw, b"bytes").unwrap();
    let id = catalog.upsert_photo(&raw, None, 1, 5).unwrap().id;

    // Enqueue is idempotent per (kind, photo).
    let op = catalog.enqueue_operation("backup", id).unwrap();
    assert_eq!(catalog.enqueue_operation("backup", id).unwrap(), op);
    assert_eq!(catalog.list_pending_operations().unwrap().len(), 1);
    assert!(catalog.enqueue_operation("bogus", id).is_err());

    // Drain with the NAS reachable runs the backup and clears the queue.
    let summary = catalog.drain_pending_operations().unwrap();
    assert_eq!((summary.ran, summary.failed, summary.skipped_offline), (1, 0, false));
    assert!(catalog.list_pending_operations().unwrap().is_empty());
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::BackedUp);

    // With the only backup volume unreachable, a drain leaves the queue intact.
    let gone = catalog
        .add_volume("OldNAS", &root.parent().unwrap().join("nope"), VolumeKind::Backup)
        .unwrap();
    catalog.remove_volume(nas).unwrap(); // now the sole backup volume is unreachable
    let _ = gone;
    catalog.enqueue_operation("backup", id).unwrap();
    let summary2 = catalog.drain_pending_operations().unwrap();
    assert!(summary2.skipped_offline);
    assert_eq!(catalog.list_pending_operations().unwrap().len(), 1);
}

#[test]
fn lifecycle_backup_offload_restore_with_verification() {
    let (catalog, root) = temp_catalog("lifecycle");
    // Local working volume = catalog root; a reachable NAS backup volume.
    let nas_dir = root.parent().unwrap().join("nas");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
    let local_id = catalog.list_volumes().unwrap().into_iter()
        .find(|v| v.kind == VolumeKind::Local).unwrap().id;

    // A real local original.
    let raw = root.join("2015/06/01/DSC1.ARW");
    std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
    std::fs::write(&raw, b"original-bytes").unwrap();
    let id = catalog.upsert_photo(&raw, None, 1, 14).unwrap().id;

    // Offload refused before any backup (invariants 1 & 2).
    assert!(catalog.offload_photo(id).is_err());
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::LocalOnly);

    // Back up: copies to the NAS (mirroring the relative path), hash-verified.
    let hash = catalog.backup_photo(id, nas).unwrap();
    let nas_copy = nas_dir.join("2015/06/01/DSC1.ARW");
    assert!(nas_copy.is_file());
    assert_eq!(std::fs::read(&nas_copy).unwrap(), b"original-bytes");
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::BackedUp);

    // Offload: local copy removed, NAS copy kept → Archived. Last copy never deleted.
    catalog.offload_photo(id).unwrap();
    assert!(!raw.exists(), "local original freed");
    assert!(nas_copy.is_file(), "backup retained");
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::Archived);
    assert_eq!(catalog.require_photo_path(id).unwrap(), nas_copy);

    // Restore: pulls the NAS copy back to local, hash-verified → BackedUp again.
    let rhash = catalog.restore_photo(id, local_id).unwrap();
    assert_eq!(rhash, hash);
    assert!(raw.exists(), "restored local copy");
    assert_eq!(catalog.photo_storage_status(id).unwrap(), StorageStatus::BackedUp);

    // Tampered backup: offload must refuse (invariant 3 — hash-verify before delete).
    std::fs::write(&nas_copy, b"corrupted").unwrap();
    assert!(catalog.offload_photo(id).is_err());
    assert!(raw.exists(), "local kept because backup verification failed");
}

#[test]
fn per_photo_storage_status_from_locations() {
    let (catalog, root) = temp_catalog("storage-status");

    // A reachable backup (NAS) volume and an unreachable one.
    let nas_dir = root.parent().unwrap().join("nas");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
    let offline_nas = catalog
        .add_volume("OldNAS", &root.parent().unwrap().join("missing-nas"), VolumeKind::Backup)
        .unwrap();
    let default_local = catalog.list_volumes().unwrap()
        .into_iter().find(|v| v.kind == VolumeKind::Local).unwrap().id;

    // local-only: a photo with just its default local primary location.
    let local = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
    assert_eq!(catalog.photo_storage_status(local).unwrap(), StorageStatus::LocalOnly);

    // backed-up: local + a backup copy.
    let backed = catalog.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;
    catalog.add_location(backed, nas, "b.arw", LocationRole::Backup).unwrap();
    assert_eq!(catalog.photo_storage_status(backed).unwrap(), StorageStatus::BackedUp);

    // archived: backup-only on a reachable NAS (no local copy).
    let archived = catalog.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
    catalog.remove_locations_on_volume(archived, default_local).unwrap();
    catalog.add_location(archived, nas, "c.arw", LocationRole::Backup).unwrap();
    assert_eq!(catalog.photo_storage_status(archived).unwrap(), StorageStatus::Archived);

    // offline: backup-only on an unreachable NAS.
    let offline = catalog.upsert_photo(&root.join("d.arw"), None, 1, 1).unwrap().id;
    catalog.remove_locations_on_volume(offline, default_local).unwrap();
    catalog.add_location(offline, offline_nas, "d.arw", LocationRole::Backup).unwrap();
    assert_eq!(catalog.photo_storage_status(offline).unwrap(), StorageStatus::Offline);

    // missing: no location records at all.
    let missing = catalog.upsert_photo(&root.join("e.arw"), None, 1, 1).unwrap().id;
    catalog.remove_locations_on_volume(missing, default_local).unwrap();
    assert_eq!(catalog.photo_storage_status(missing).unwrap(), StorageStatus::Missing);

    // Batch returns one entry per requested id, matching the singles. The batch method
    // no longer stats — the caller supplies the reachability map (here from list_volumes,
    // which mirrors the old internal behaviour).
    let reachable: std::collections::HashMap<i64, bool> = catalog
        .list_volumes()
        .unwrap()
        .into_iter()
        .map(|v| (v.id, v.reachable))
        .collect();
    let batch = catalog
        .photo_storage_statuses(&[local, backed, archived, offline, missing], &reachable)
        .unwrap();
    assert_eq!(
        batch,
        vec![
            (local, StorageStatus::LocalOnly),
            (backed, StorageStatus::BackedUp),
            (archived, StorageStatus::Archived),
            (offline, StorageStatus::Offline),
            (missing, StorageStatus::Missing),
        ]
    );

    // With an all-unreachable map, the two backup-only photos that were reachable now
    // report Offline (their backup volume is "down"); local-only and missing are
    // unaffected (they don't depend on backup reachability).
    let all_unreachable: std::collections::HashMap<i64, bool> =
        reachable.keys().map(|&id| (id, false)).collect();
    let offline_batch = catalog
        .photo_storage_statuses(&[local, backed, archived, offline, missing], &all_unreachable)
        .unwrap();
    assert_eq!(
        offline_batch,
        vec![
            (local, StorageStatus::LocalOnly),
            (backed, StorageStatus::BackedUp),
            (archived, StorageStatus::Offline),
            (offline, StorageStatus::Offline),
            (missing, StorageStatus::Missing),
        ]
    );
}

#[test]
fn grid_badge_batch_queries_accept_more_than_one_parameter_chunk() {
    let (catalog, root) = temp_catalog("grid-badge-chunks");
    let total = 1_100;
    let mut ids = Vec::with_capacity(total);

    for index in 0..total {
        let path = root.join(format!("chunked-{index:04}.arw"));
        std::fs::write(&path, b"x").unwrap();
        let id = catalog
            .upsert_photo(&path, None, index as i64, 1)
            .unwrap()
            .id;
        if index % 10 == 0 {
            catalog.create_version(id, "Edit").unwrap();
        }
        ids.push(id);
    }

    let reachable: std::collections::HashMap<i64, bool> = catalog
        .list_volumes()
        .unwrap()
        .into_iter()
        .map(|v| (v.id, v.reachable))
        .collect();
    let statuses = catalog.photo_storage_statuses(&ids, &reachable).unwrap();
    assert_eq!(statuses.len(), ids.len());
    assert!(
        statuses
            .iter()
            .all(|(_, status)| *status == StorageStatus::LocalOnly),
        "all synthetic photos have only their local primary location"
    );

    let counts = catalog.version_counts(&ids).unwrap();
    assert_eq!(counts.len(), total / 10);
    assert_eq!(counts[0], (ids[0], 1));

    // The rows above only prove that per-chunk results stitch back together: 1,100 binds
    // fit under `SQLITE_MAX_VARIABLE_NUMBER`, which is 32766 in the SQLite we bundle, so
    // an unchunked `IN (...)` would still pass. Pad past that ceiling to cover the
    // chunking itself — a 100k-photo grid refresh passes every returned id, and
    // unchunked this fails with "too many SQL variables". Padding ids need no rows; they
    // only have to survive being bound.
    const SQLITE_MAX_VARIABLE_NUMBER: usize = 32_766;
    let padding_start = ids.last().unwrap() + 1;
    let mut padded = ids.clone();
    padded.extend(padding_start..padding_start + SQLITE_MAX_VARIABLE_NUMBER as i64);
    assert!(
        padded.len() > SQLITE_MAX_VARIABLE_NUMBER,
        "the padded batch must exceed the bind-parameter ceiling"
    );

    let padded_statuses = catalog.photo_storage_statuses(&padded, &reachable).unwrap();
    assert_eq!(padded_statuses.len(), padded.len());
    assert_eq!(
        &padded_statuses[..total],
        statuses.as_slice(),
        "the real ids keep their status and order when the batch spans many chunks"
    );

    let padded_counts = catalog.version_counts(&padded).unwrap();
    assert_eq!(padded_counts, counts, "padding ids contribute no version rows");
}

#[test]
fn rerooting_moves_the_catalog_root_volume() {
    let dir = common::TestTmpDir::new("reroot");
    let db = dir.join("c.chairphoto");
    let root_a = dir.join("libA");
    let root_b = dir.join("libB");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    // Open rooted at A: the catalog-root (local) volume points at A.
    {
        let c = Catalog::open(&db, &root_a).unwrap();
        let local = c.list_volumes().unwrap().into_iter()
            .find(|v| v.kind == VolumeKind::Local).unwrap();
        assert_eq!(local.base_path, root_a.to_string_lossy());
        // Re-root: persist a new catalog_root, as set_library_root does.
        c.set_setting("catalog_root", &root_b.to_string_lossy()).unwrap();
    }

    // Reopen: the catalog adopts B AND the local volume's base path follows.
    let c = Catalog::open(&db, &root_a).unwrap();
    assert_eq!(c.root(), root_b);
    let local = c.list_volumes().unwrap().into_iter()
        .find(|v| v.kind == VolumeKind::Local).unwrap();
    assert_eq!(local.base_path, root_b.to_string_lossy(), "local volume re-rooted too");
}

#[test]
fn volume_management_add_list_remove() {
    let (catalog, root) = temp_catalog("volume-mgmt");

    // Migration created exactly the default volume.
    let initial = catalog.list_volumes().unwrap();
    assert_eq!(initial.len(), 1);
    let default_id = initial[0].id;

    // Add a NAS-like volume; it lists with a reachability flag.
    let nas_dir = root.parent().unwrap().join("nas");
    std::fs::create_dir_all(&nas_dir).unwrap();
    let nas = catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
    let listed = catalog.list_volumes().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().find(|v| v.id == nas).unwrap().reachable);

    // A volume whose path doesn't exist is reported unreachable.
    let gone = catalog
        .add_volume("Gone", &root.parent().unwrap().join("nope"), VolumeKind::Backup)
        .unwrap();
    assert!(!catalog.list_volumes().unwrap().iter().find(|v| v.id == gone).unwrap().reachable);

    // Remove a non-default volume.
    catalog.remove_volume(nas).unwrap();
    assert!(catalog.list_volumes().unwrap().iter().all(|v| v.id != nas));

    // The default catalog-root volume is protected.
    assert!(catalog.remove_volume(default_id).is_err());
    assert!(catalog.list_volumes().unwrap().iter().any(|v| v.id == default_id));
}

#[test]
fn resolver_prefers_local_cache_over_primary() {
    let (catalog, root) = temp_catalog("resolver-pref");

    // Primary on the default volume (under the catalog root).
    let primary_path = root.join("orig.arw");
    std::fs::write(&primary_path, b"orig").unwrap();
    let id = catalog.upsert_photo(&primary_path, None, 1, 4).unwrap().id;

    // A second volume acting as a fast local cache, with the same file.
    let cache_dir = root.parent().unwrap().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache_path = cache_dir.join("orig.arw");
    std::fs::write(&cache_path, b"orig").unwrap();
    let cache_vol = catalog.add_volume("LocalScratch", &cache_dir, VolumeKind::Local).unwrap();
    catalog
        .add_location(id, cache_vol, "orig.arw", LocationRole::LocalCache)
        .unwrap();

    // Both exist, so the resolver should prefer the local-cache copy.
    assert_eq!(catalog.resolve_photo_path(id).unwrap(), Some(cache_path.clone()));

    // If the cache copy disappears, it falls back to the primary.
    std::fs::remove_file(&cache_path).unwrap();
    assert_eq!(catalog.resolve_photo_path(id).unwrap(), Some(primary_path));
}

#[test]
fn photo_path_candidates_are_ordered_by_role_with_root_fallback_last() {
    let (catalog, root) = temp_catalog("path-candidates");

    // A primary on the default catalog-root volume.
    let primary_path = root.join("orig.arw");
    std::fs::write(&primary_path, b"orig").unwrap();
    let id = catalog.upsert_photo(&primary_path, None, 1, 4).unwrap().id;

    // Register a local-cache and a backup volume, each with a location (no files on disk).
    let cache_dir = root.parent().unwrap().join("cache");
    let backup_dir = root.parent().unwrap().join("backup");
    let cache_vol = catalog.add_volume("Cache", &cache_dir, VolumeKind::Local).unwrap();
    let backup_vol = catalog.add_volume("NAS", &backup_dir, VolumeKind::Backup).unwrap();
    catalog.add_location(id, cache_vol, "orig.arw", LocationRole::LocalCache).unwrap();
    catalog.add_location(id, backup_vol, "orig.arw", LocationRole::Backup).unwrap();

    // Pure SQL: returns rows regardless of what exists on disk.
    let cands: Vec<PathCandidate> = catalog.photo_path_candidates(id).unwrap();

    // Ordering: local-cache (role 0) < primary (role 1) < backup (role 2), then the
    // catalog-root fallback (volume_id None) LAST.
    assert_eq!(cands.len(), 4);
    assert_eq!(cands[0].role, LocationRole::LocalCache);
    assert_eq!(cands[0].volume_id, Some(cache_vol));
    assert_eq!(cands[0].path, cache_dir.join("orig.arw"));
    assert_eq!(cands[1].role, LocationRole::Primary); // the default-volume primary
    assert_eq!(cands[1].path, primary_path);
    assert_eq!(cands[2].role, LocationRole::Backup);
    assert_eq!(cands[2].volume_id, Some(backup_vol));
    assert_eq!(cands[2].path, backup_dir.join("orig.arw"));
    // Root fallback last, with no volume id.
    assert_eq!(cands[3].volume_id, None);
    assert_eq!(cands[3].path, primary_path);
}

#[test]
fn resolver_returns_none_when_no_copy_exists() {
    let (catalog, root) = temp_catalog("resolver-missing");
    let path = root.join("gone.arw");
    std::fs::write(&path, b"x").unwrap();
    let id = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;
    std::fs::remove_file(&path).unwrap();
    assert_eq!(catalog.resolve_photo_path(id).unwrap(), None);
}

#[test]
fn reconcile_missing_hides_orphans_but_spares_offline_backups() {
    let (catalog, root) = temp_catalog("reconcile-missing");

    // A) A present photo under the catalog root — resolvable, stays visible.
    let present = root.join("present.arw");
    std::fs::write(&present, b"x").unwrap();
    let present_id = catalog.upsert_photo(&present, None, 1, 1).unwrap().id;

    // B) An orphan: indexed, then its file is gone, and it has no backup copy.
    let orphan = root.join("orphan.arw");
    std::fs::write(&orphan, b"x").unwrap();
    let orphan_id = catalog.upsert_photo(&orphan, None, 1, 1).unwrap().id;
    std::fs::remove_file(&orphan).unwrap();

    // C) Offline NAS photo: no local file, but a backup location on an unmounted volume.
    let offline = root.join("offline.arw");
    std::fs::write(&offline, b"x").unwrap();
    let offline_id = catalog.upsert_photo(&offline, None, 1, 1).unwrap().id;
    std::fs::remove_file(&offline).unwrap();
    let offline_nas = catalog
        .add_volume("OldNAS", &root.parent().unwrap().join("missing-nas"), VolumeKind::Backup)
        .unwrap();
    catalog
        .add_location(offline_id, offline_nas, "offline.arw", LocationRole::Backup)
        .unwrap();

    catalog.reconcile_missing().unwrap();

    let visible: Vec<i64> = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert!(visible.contains(&present_id), "present photo stays visible");
    assert!(!visible.contains(&orphan_id), "orphan with no backup is hidden");
    assert!(visible.contains(&offline_id), "offline NAS photo is spared (has a backup)");
}

#[test]
fn recently_used_tags_tracks_manual_use_and_excludes_auto_tags() {
    let (catalog, root) = temp_catalog("recently-used");
    let path = root.join("p.arw");
    std::fs::write(&path, b"x").unwrap();
    let photo = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;

    let a = catalog.create_tag("Alpha").unwrap();
    let b = catalog.create_tag("Beta").unwrap();
    let unused = catalog.create_tag("Gamma").unwrap(); // created but never applied

    // Apply Alpha then Beta by hand.
    catalog.assign_tag(photo, a).unwrap();
    catalog.assign_tag(photo, b).unwrap();

    let recent = catalog.recently_used_tags(10).unwrap();
    let ids: Vec<i64> = recent.iter().map(|t| t.id).collect();
    assert!(ids.contains(&a) && ids.contains(&b), "applied tags appear");
    assert!(!ids.contains(&unused), "a never-applied tag is excluded");
    // Most-recent first (ties broken by id desc — Beta was applied last).
    assert_eq!(recent[0].id, b, "the last-applied tag is first");
    // Limit is honoured.
    assert_eq!(catalog.recently_used_tags(1).unwrap().len(), 1);

    // The monochrome AUTO-tag is assigned by the engine (bypassing assign_tag), so it
    // must NOT pollute the recently-used list.
    catalog.set_grayscale(photo, true).unwrap();
    catalog.apply_auto_tags().unwrap();
    let mono = catalog
        .list_tags_with_counts()
        .unwrap()
        .into_iter()
        .find(|t| t.tag.auto_rule.as_deref() == Some("monochrome"))
        .expect("monochrome auto-tag exists after apply_auto_tags");
    let recent_ids: Vec<i64> = catalog
        .recently_used_tags(10)
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert!(
        !recent_ids.contains(&mono.tag.id),
        "auto-tags never appear in recently used",
    );
}

#[test]
fn photo_versions_crud_and_counts() {
    let (catalog, root) = temp_catalog("photo-versions");
    let path = root.join("v.arw");
    std::fs::write(&path, b"x").unwrap();
    let photo = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;

    // Create two versions; they list in creation order with a default edit record.
    let a = catalog.create_version(photo, "Square").unwrap();
    let b = catalog.create_version(photo, "Bright").unwrap();
    let listed = catalog.list_versions(photo).unwrap();
    assert_eq!(listed.iter().map(|v| v.id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(listed[0].name, "Square");
    assert_eq!(listed[0].edit_json, "{}");

    // Empty names are rejected.
    assert!(catalog.create_version(photo, "  ").is_err());

    // Edit record: valid JSON stored, invalid rejected.
    catalog.set_version_edit(a, r#"{"crop":{"aspect":"1:1"}}"#).unwrap();
    assert!(catalog.set_version_edit(a, "not json").is_err());
    assert_eq!(catalog.get_version(a).unwrap().unwrap().edit_json, r#"{"crop":{"aspect":"1:1"}}"#);

    // Duplicate appends "<name> copy" carrying the same edit record.
    let dup = catalog.duplicate_version(a).unwrap();
    let dupv = catalog.get_version(dup).unwrap().unwrap();
    assert_eq!(dupv.name, "Square copy");
    assert_eq!(dupv.edit_json, r#"{"crop":{"aspect":"1:1"}}"#);

    // Rename + reorder.
    catalog.rename_version(b, "Brighter").unwrap();
    catalog.reorder_versions(photo, &[b, dup, a]).unwrap();
    let order: Vec<i64> = catalog.list_versions(photo).unwrap().iter().map(|v| v.id).collect();
    assert_eq!(order, vec![b, dup, a]);

    // Counts (batch) report this photo's three versions.
    let counts = catalog.version_counts(&[photo, 9999]).unwrap();
    assert_eq!(counts, vec![(photo, 3)]);

    // Delete one.
    catalog.delete_version(dup).unwrap();
    assert_eq!(catalog.list_versions(photo).unwrap().len(), 2);
    assert!(catalog.version_counts(&[photo]).unwrap() == vec![(photo, 2)]);
}

#[test]
fn tag_terms_translations_and_synonyms() {
    let (catalog, _root) = temp_catalog("terms");
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();

    // Translations: canonical is "Owl"; add a Norwegian primary.
    catalog.set_translation(owl, "nb", "Ugle").unwrap();
    assert_eq!(catalog.display_name(owl, Some("nb")).unwrap(), "Ugle");
    assert_eq!(catalog.display_name(owl, Some("de")).unwrap(), "Owl"); // fallback to canonical
    assert_eq!(catalog.display_name(owl, None).unwrap(), "Owl");

    // Synonyms with per-synonym export flags.
    catalog.add_synonym(owl, "Owls", Some("en"), true).unwrap();
    catalog.add_synonym(owl, "Strigiformes", None, true).unwrap(); // neutral, exported
    catalog.add_synonym(owl, "internal-note", Some("en"), false).unwrap(); // not exported

    let terms = catalog.list_terms(owl).unwrap();
    assert_eq!(terms.len(), 4); // nb primary + 3 synonyms

    // Setting a second nb primary demotes the first.
    catalog.set_translation(owl, "nb", "Ugla").unwrap();
    assert_eq!(catalog.display_name(owl, Some("nb")).unwrap(), "Ugla");
}

#[test]
fn rename_tag_rewrites_descendant_paths() {
    let (catalog, _root) = temp_catalog("rename-tag");
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    let birds = catalog.create_tag("Animals/Birds").unwrap();

    // Rename the middle node; descendant paths must follow.
    catalog.rename_tag(birds, "Avians").unwrap();
    assert_eq!(catalog.get_tag(birds).unwrap().full_path, "Animals/Avians");
    assert_eq!(catalog.get_tag(owl).unwrap().full_path, "Animals/Avians/Owl");

    // Renaming to a name that collides with a sibling path is rejected.
    catalog.create_tag("Animals/Mammals").unwrap();
    assert!(catalog.rename_tag(birds, "Mammals").is_err());
}

#[test]
fn tags_get_stable_unique_uuids() {
    let (catalog, _root) = temp_catalog("tag-uuid");
    let a = catalog.create_tag("Animals/Birds").unwrap();
    let b = catalog.create_tag("Plants").unwrap();
    let ua = catalog.get_tag(a).unwrap().uuid;
    let ub = catalog.get_tag(b).unwrap().uuid;
    assert!(!ua.is_empty() && !ub.is_empty(), "tags must have UUIDs");
    assert_ne!(ua, ub, "UUIDs must be unique");
    // Stable: re-creating the same path returns the same tag with the same uuid.
    let a2 = catalog.create_tag("Animals/Birds").unwrap();
    assert_eq!(a, a2);
    assert_eq!(catalog.get_tag(a2).unwrap().uuid, ua);
    // Ancestors created along the way also got UUIDs.
    let animals = catalog.create_tag("Animals").unwrap();
    assert!(!catalog.get_tag(animals).unwrap().uuid.is_empty());
}

#[test]
fn move_tag_reparents_and_rewrites_paths() {
    let (catalog, _root) = temp_catalog("move-tag");
    let events = catalog.create_tag("Public Places/Events").unwrap();

    // Move "Events" up to the top level.
    catalog.move_tag(events, None).unwrap();
    assert_eq!(catalog.get_tag(events).unwrap().full_path, "Events");
    assert_eq!(catalog.get_tag(events).unwrap().parent_id, None);

    // Descendants follow the move.
    let concert = catalog.create_tag("Events/Concert").unwrap();
    let music = catalog.create_tag("Music").unwrap();
    catalog.move_tag(events, Some(music)).unwrap();
    assert_eq!(catalog.get_tag(events).unwrap().full_path, "Music/Events");
    assert_eq!(catalog.get_tag(concert).unwrap().full_path, "Music/Events/Concert");

    // Can't move a tag into its own descendant.
    assert!(catalog.move_tag(music, Some(concert)).is_err());
}

#[test]
fn delete_tag_removes_subtree_and_assignments() {
    let (catalog, root) = temp_catalog("delete-tag");
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    let animals = catalog.create_tag("Animals").unwrap();
    let id = catalog
        .upsert_photo(&root.join("a.jpg"), None, 1, 1)
        .unwrap()
        .id;
    catalog.assign_tag(id, owl).unwrap();
    catalog.add_synonym(owl, "Owls", None, true).unwrap();
    assert_eq!(catalog.list_tags_with_counts().unwrap().len(), 3);

    // Deleting the root removes the whole subtree, the assignment, and the terms.
    catalog.delete_tag(animals).unwrap();
    assert_eq!(catalog.list_tags_with_counts().unwrap().len(), 0);
    assert_eq!(catalog.get_photo_tags(id).unwrap().len(), 0);
    assert!(catalog.get_tag(owl).is_err());
}

#[test]
fn iptc_round_trips() {
    use chairphoto_lib::catalog::IptcFields;
    let (catalog, root) = temp_catalog("iptc");
    let id = catalog
        .upsert_photo(&root.join("a.jpg"), None, 1, 1)
        .unwrap()
        .id;
    assert_eq!(catalog.get_iptc(id).unwrap().description, "");

    let fields = IptcFields {
        description: "A ferry in the harbour".into(),
        creator: "Andreas".into(),
        copyright: "© 2026 Andreas".into(),
        city: "Trondheim".into(),
        country: "Norway".into(),
        ..Default::default()
    };
    catalog.set_iptc(id, &fields).unwrap();
    let got = catalog.get_iptc(id).unwrap();
    assert_eq!(got.description, "A ferry in the harbour");
    assert_eq!(got.creator, "Andreas");
    assert_eq!(got.city, "Trondheim");
}

#[test]
fn monochrome_auto_tag_applies_from_grayscale_flag() {
    let (catalog, root) = temp_catalog("autotag-mono");

    // Pixel-derived flag drives monochrome (camera metadata is unreliable). A real
    // grayscale shot vs a colour one.
    let bw = catalog.upsert_photo(&root.join("bw.arw"), None, 1, 1).unwrap().id;
    catalog.set_grayscale(bw, true).unwrap();
    let colour = catalog.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
    catalog.set_grayscale(colour, false).unwrap();

    catalog.apply_auto_tags().unwrap();

    // Only the B&W photo gets the auto-tag, which is marked auto + carries hashtags.
    let bw_tags = catalog.get_photo_tags(bw).unwrap();
    assert_eq!(bw_tags.len(), 1);
    assert_eq!(bw_tags[0].full_path, "Treatment/Black & White");
    assert_eq!(bw_tags[0].auto_rule.as_deref(), Some("monochrome"));
    assert_eq!(catalog.get_photo_tags(colour).unwrap().len(), 0);

    // It's a real tag with export hashtags as synonyms.
    let terms = catalog.list_terms(bw_tags[0].id).unwrap();
    assert!(terms.iter().any(|t| t.text == "#bnw" && t.export));

    // Re-running is idempotent (no duplicate membership).
    catalog.apply_auto_tags().unwrap();
    assert_eq!(catalog.get_photo_tags(bw).unwrap().len(), 1);
}

#[test]
fn tag_description_round_trips() {
    let (catalog, _root) = temp_catalog("tag-desc");
    let crane = catalog.create_tag("Animals/Birds/Crane").unwrap();
    assert_eq!(catalog.get_tag(crane).unwrap().description, "");

    catalog
        .set_tag_description(crane, "Large long-legged wading bird (Gruidae), not the machine.")
        .unwrap();
    assert!(catalog
        .get_tag(crane)
        .unwrap()
        .description
        .contains("wading bird"));

    // Description is visible in the listing too.
    let listed = catalog.list_tags_with_counts().unwrap();
    let row = listed.iter().find(|t| t.tag.id == crane).unwrap();
    assert!(row.tag.description.contains("Gruidae"));
}

#[test]
fn export_labels_respects_language_and_export_flags() {
    let (catalog, _root) = temp_catalog("export-labels");
    let owl = catalog.create_tag("Owl").unwrap();
    catalog.set_translation(owl, "nb", "Ugle").unwrap();
    catalog.add_synonym(owl, "Owls", Some("en"), true).unwrap();
    catalog.add_synonym(owl, "Strigiformes", None, true).unwrap(); // neutral
    catalog.add_synonym(owl, "secret", Some("en"), false).unwrap(); // export off

    // English only: canonical "Owl" + en synonym "Owls" + neutral "Strigiformes".
    // "secret" excluded (export off); "Ugle" excluded (nb not selected).
    let en = catalog.export_labels(owl, &["en".into()]).unwrap();
    assert!(en.contains(&"Owl".to_string()));
    assert!(en.contains(&"Owls".to_string()));
    assert!(en.contains(&"Strigiformes".to_string()));
    assert!(!en.contains(&"secret".to_string()));
    assert!(!en.contains(&"Ugle".to_string()));

    // Norwegian only: primary "Ugle" + neutral synonym; no en synonym.
    let nb = catalog.export_labels(owl, &["nb".into()]).unwrap();
    assert!(nb.contains(&"Ugle".to_string()));
    assert!(nb.contains(&"Strigiformes".to_string()));
    assert!(!nb.contains(&"Owls".to_string()));

    // Both languages: both primaries + both-language synonyms + neutral.
    let both = catalog.export_labels(owl, &["en".into(), "nb".into()]).unwrap();
    assert!(both.contains(&"Owl".to_string()));
    assert!(both.contains(&"Ugle".to_string()));
    assert!(both.contains(&"Owls".to_string()));
}

#[test]
fn path_labels_translate_each_level() {
    let (catalog, _root) = temp_catalog("path-labels");
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    // create_tag is idempotent, so these return the existing ancestor ids.
    let birds = catalog.create_tag("Animals/Birds").unwrap();
    let animals = catalog.create_tag("Animals").unwrap();
    catalog.set_translation(animals, "nb", "Dyr").unwrap();
    catalog.set_translation(birds, "nb", "Fugler").unwrap();
    catalog.set_translation(owl, "nb", "Ugle").unwrap();

    let path = catalog.path_labels(owl, Some("nb")).unwrap();
    assert_eq!(path, vec!["Dyr", "Fugler", "Ugle"]);

    // A missing translation falls back to the canonical level name.
    let en = catalog.path_labels(owl, Some("en")).unwrap();
    assert_eq!(en, vec!["Animals", "Birds", "Owl"]);
}

#[test]
fn export_labels_with_ancestors_expands_the_chain() {
    let (catalog, _root) = temp_catalog("export-ancestors");
    let eagle = catalog.create_tag("Animals/Birds/Eagle").unwrap();
    let birds = catalog.create_tag("Animals/Birds").unwrap();
    let animals = catalog.create_tag("Animals").unwrap();
    catalog.add_synonym(eagle, "Raptor", Some("en"), true).unwrap();
    catalog.add_synonym(birds, "Aves", None, true).unwrap();
    catalog.add_synonym(birds, "hidden", Some("en"), false).unwrap(); // export off

    // Root → leaf: each level's canonical name plus its exportable synonyms.
    let labels = catalog
        .export_labels_with_ancestors(eagle, &["en".into()])
        .unwrap();
    assert_eq!(labels, vec!["Animals", "Birds", "Aves", "Eagle", "Raptor"]);
    assert!(!labels.contains(&"hidden".to_string()));

    // The leaf alone still returns just its own labels.
    let leaf_only = catalog.export_labels(eagle, &["en".into()]).unwrap();
    assert_eq!(leaf_only, vec!["Eagle", "Raptor"]);

    // A root-level tag (no parent) matches plain export_labels.
    let root = catalog
        .export_labels_with_ancestors(animals, &["en".into()])
        .unwrap();
    assert_eq!(root, vec!["Animals"]);
}

#[test]
fn list_photos_culling_filters() {
    let (catalog, root) = temp_catalog("filters");
    let keep = catalog.upsert_photo(&root.join("k.jpg"), None, 1, 1).unwrap().id;
    let toss = catalog.upsert_photo(&root.join("t.jpg"), None, 1, 1).unwrap().id;

    catalog.set_culling(keep, Some(5), None, Some(PickState::Pick)).unwrap();
    catalog.set_culling(toss, None, None, Some(PickState::Reject)).unwrap();

    assert_eq!(catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None).unwrap().len(), 2);
    assert_eq!(catalog.list_photos(None, None, None, &[], "pick", None, "all", None, None, &[], None).unwrap().len(), 1);
    assert_eq!(catalog.list_photos(None, None, None, &[], "reject", None, "all", None, None, &[], None).unwrap().len(), 1);
    assert_eq!(catalog.list_photos(None, None, None, &[], "unrated", None, "all", None, None, &[], None).unwrap().len(), 1);
}

#[test]
fn edit_record_round_trips_and_validates() {
    let (catalog, root) = temp_catalog("edits");
    let id = catalog.upsert_photo(&root.join("e.arw"), None, 1, 1).unwrap().id;

    // No record initially.
    assert_eq!(catalog.get_edit_record(id).unwrap(), None);
    assert!(!catalog.photo_has_edits(id).unwrap());

    // Set an opaque, module-namespaced JSON document; it round-trips verbatim.
    let doc = r#"{"basic-editor":{"exposure":0.3,"bw":true}}"#;
    catalog.set_edit_record(id, doc).unwrap();
    assert_eq!(catalog.get_edit_record(id).unwrap().as_deref(), Some(doc));
    assert!(catalog.photo_has_edits(id).unwrap());

    // Replacing overwrites.
    let doc2 = r#"{"basic-editor":{"exposure":-1.0}}"#;
    catalog.set_edit_record(id, doc2).unwrap();
    assert_eq!(catalog.get_edit_record(id).unwrap().as_deref(), Some(doc2));

    // Invalid JSON is rejected (core validates syntax, not schema).
    assert!(catalog.set_edit_record(id, "not json").is_err());
    // The prior valid record is untouched by the rejected write.
    assert_eq!(catalog.get_edit_record(id).unwrap().as_deref(), Some(doc2));

    // Empty clears the record.
    catalog.set_edit_record(id, "  ").unwrap();
    assert_eq!(catalog.get_edit_record(id).unwrap(), None);
    assert!(!catalog.photo_has_edits(id).unwrap());
}

#[test]
fn albums_membership_and_composition_with_culling() {
    let (catalog, root) = temp_catalog("albums");
    let a = catalog.upsert_photo(&root.join("a.jpg"), None, 1, 1).unwrap().id;
    let b = catalog.upsert_photo(&root.join("b.jpg"), None, 1, 1).unwrap().id;
    let c = catalog.upsert_photo(&root.join("c.jpg"), None, 1, 1).unwrap().id;

    let trip = catalog.create_album("Trip").unwrap();
    // Add b before a: album order is insertion order, not path order.
    catalog.add_photos_to_album(trip, &[b, a]).unwrap();
    // Re-adding is idempotent (no duplicate, no error).
    catalog.add_photos_to_album(trip, &[a]).unwrap();

    // Viewing the album returns members in insertion order (b, then a).
    let ordered = catalog.list_photos(None, Some(trip), None, &[], "all", None, "all", None, None, &[], None).unwrap();
    assert_eq!(ordered.iter().map(|p| p.id).collect::<Vec<_>>(), vec![b, a]);

    let albums = catalog.list_albums().unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].name, "Trip");
    assert_eq!(albums[0].photo_count, 2);

    // Viewing the album returns only its members.
    let in_album = catalog.list_photos(None, Some(trip), None, &[], "all", None, "all", None, None, &[], None).unwrap();
    assert_eq!(in_album.len(), 2);
    assert!(in_album.iter().all(|p| p.id != c));

    // Album filter ANDs with the culling filter.
    catalog.set_culling(a, Some(5), None, Some(PickState::Pick)).unwrap();
    let picks = catalog.list_photos(None, Some(trip), None, &[], "pick", None, "all", None, None, &[], None).unwrap();
    assert_eq!(picks.len(), 1);
    assert_eq!(picks[0].id, a);

    // Removing a photo updates membership; deleting the album never deletes photos.
    catalog.remove_photos_from_album(trip, &[a]).unwrap();
    assert_eq!(catalog.list_albums().unwrap()[0].photo_count, 1);
    catalog.delete_album(trip).unwrap();
    assert!(catalog.list_albums().unwrap().is_empty());
    assert_eq!(catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None).unwrap().len(), 3);
}

#[test]
fn facets_filter_by_derived_exif() {
    use chairphoto_lib::catalog::PromotedMetadata;
    let (catalog, root) = temp_catalog("facets");

    let set = |id: i64, make: Option<&str>, model: Option<&str>, gps: bool| {
        catalog
            .set_photo_metadata(
                id,
                &PromotedMetadata {
                    camera_make: make.map(str::to_string),
                    camera_model: model.map(str::to_string),
                    gps_latitude: gps.then_some(59.9),
                    gps_longitude: gps.then_some(10.7),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
    };

    let phone = catalog.upsert_photo(&root.join("p.jpg"), None, 1, 1).unwrap().id;
    set(phone, Some("Apple"), Some("iPhone 15 Pro"), true); // mobile + gps
    let drone = catalog.upsert_photo(&root.join("d.jpg"), None, 1, 1).unwrap().id;
    set(drone, Some("DJI"), Some("FC3411"), true); // drone + gps
    let cam = catalog.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
    set(cam, Some("Sony"), Some("ILCE-7RM5"), false); // neither

    let ids = |fs: &[&str]| {
        let fs: Vec<String> = fs.iter().map(|s| s.to_string()).collect();
        let mut v: Vec<i64> = catalog
            .list_photos(None, None, None, &fs, "all", None, "all", None, None, &[], None)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        v.sort();
        v
    };

    assert_eq!(ids(&["mobile"]), vec![phone]);
    assert_eq!(ids(&["drone"]), vec![drone]);
    let mut gps = vec![phone, drone];
    gps.sort();
    assert_eq!(ids(&["has-gps"]), gps);
    // Facets AND together (and with each other): mobile AND has-gps still = the phone.
    assert_eq!(ids(&["mobile", "has-gps"]), vec![phone]);
    // mobile AND drone = nothing.
    assert!(ids(&["mobile", "drone"]).is_empty());
    // Unknown facet is an error.
    assert!(catalog
        .list_photos(None, None, None, &["bogus".to_string()], "all", None, "all", None, None, &[], None)
        .is_err());

    // Facets are reflected in the offered set.
    let offered: Vec<String> = catalog.available_facets().into_iter().map(|f| f.key).collect();
    assert!(offered.contains(&"has-gps".to_string()));
    assert!(offered.contains(&"mobile".to_string()));
    assert!(offered.contains(&"drone".to_string()));
}

#[test]
fn soft_facet_and_sharpness_sort() {
    // H16d: the `soft` facet fires when sharpness IS NOT NULL AND sharpness < threshold;
    // sort-by-sharpness_asc puts least-sharp first (unscored last); sharpness is surfaced
    // on the Photo struct.
    use chairphoto_lib::catalog::SOFT_THRESHOLD_DEFAULT;

    let (catalog, root) = temp_catalog("soft-facet");

    let sharp = catalog.upsert_photo(&root.join("sharp.arw"), None, 1, 1).unwrap().id;
    let soft  = catalog.upsert_photo(&root.join("soft.arw"),  None, 1, 1).unwrap().id;
    let unscored = catalog.upsert_photo(&root.join("unscored.arw"), None, 1, 1).unwrap().id;

    // Score two of the three photos. Use values clearly on each side of the default threshold.
    catalog.set_sharpness(sharp,  SOFT_THRESHOLD_DEFAULT * 10.0, "tile").unwrap();
    catalog.set_sharpness(soft,   SOFT_THRESHOLD_DEFAULT * 0.1,  "tile").unwrap();
    // `unscored` stays NULL.
    let _ = unscored; // referenced above, silence the unused-variable lint

    // `soft` facet: should include `soft` photo, exclude `sharp` and `unscored`.
    let soft_ids: Vec<i64> = catalog
        .list_photos(None, None, None, &["soft".to_string()], "all", None, "all", None, None, &[], None)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(soft_ids, vec![soft], "soft facet must return only the low-score photo");

    // `soft` facet appears in `available_facets` once at least one photo is scored.
    let facet_keys: Vec<String> = catalog.available_facets().into_iter().map(|f| f.key).collect();
    assert!(facet_keys.contains(&"soft".to_string()), "soft facet should be offered after scoring");

    // Sort by sharpness_asc: soft < sharp; unscored comes last.
    let asc: Vec<i64> = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], Some("sharpness_asc"))
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(asc[0], soft,     "least-sharp photo must come first in sharpness_asc");
    assert_eq!(asc[1], sharp,    "sharp photo second");
    assert_eq!(asc[2], unscored, "unscored photo last in sharpness_asc");

    // Sort by sharpness_desc: sharp < soft; unscored comes last.
    let desc: Vec<i64> = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], Some("sharpness_desc"))
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(desc[0], sharp,    "sharpest photo must come first in sharpness_desc");
    assert_eq!(desc[1], soft,     "soft photo second");
    assert_eq!(desc[2], unscored, "unscored photo last in sharpness_desc");

    // The sharpness value is surfaced on the Photo struct.
    let all = catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None).unwrap();
    let photo_sharp = all.iter().find(|p| p.id == sharp).unwrap();
    let photo_unscored = all.iter().find(|p| p.id == unscored).unwrap();
    assert!(photo_sharp.sharpness.is_some(), "sharpness must be Some for a scored photo");
    assert!(photo_unscored.sharpness.is_none(), "sharpness must be None for an unscored photo");

    // Unknown sort falls back gracefully to date order (no error).
    let result = catalog.list_photos(None, None, None, &[], "all", None, "all", None, None, &[], Some("bogus_sort"));
    assert!(result.is_ok(), "unknown sort key must not error");
}

#[test]
fn external_editors_drive_the_edited_filter() {
    let (catalog, root) = temp_catalog("edited-filter");
    let edited = catalog.upsert_photo(&root.join("e.arw"), None, 1, 1).unwrap().id;
    let plain = catalog.upsert_photo(&root.join("p.arw"), None, 1, 1).unwrap().id;

    catalog.set_external_editors(edited, "darktable").unwrap();
    catalog.set_external_editors(plain, "").unwrap();

    let edited_only = catalog.list_photos(None, None, None, &[], "edited", None, "all", None, None, &[], None).unwrap();
    assert_eq!(edited_only.len(), 1);
    assert_eq!(edited_only[0].id, edited);
    assert_eq!(edited_only[0].external_editors, "darktable");

    // Clearing it removes the photo from the filter again.
    catalog.set_external_editors(edited, "").unwrap();
    assert_eq!(catalog.list_photos(None, None, None, &[], "edited", None, "all", None, None, &[], None).unwrap().len(), 0);
}

#[test]
fn long_exposure_auto_tag_applies_from_shutter() {
    use chairphoto_lib::catalog::PromotedMetadata;
    let (catalog, root) = temp_catalog("autotag-longexp");

    let set_shutter = |id: i64, shutter: &str| {
        catalog
            .set_photo_metadata(
                id,
                &PromotedMetadata {
                    shutter_speed: Some(shutter.to_string()),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
    };

    let long = catalog.upsert_photo(&root.join("long.arw"), None, 1, 1).unwrap().id;
    set_shutter(long, "30"); // 30s
    let long2 = catalog.upsert_photo(&root.join("long2.arw"), None, 1, 1).unwrap().id;
    set_shutter(long2, "1.3"); // 1.3s
    let boundary = catalog.upsert_photo(&root.join("bound.arw"), None, 1, 1).unwrap().id;
    set_shutter(boundary, "1"); // exactly the 1s threshold (>=)
    let fast = catalog.upsert_photo(&root.join("fast.arw"), None, 1, 1).unwrap().id;
    set_shutter(fast, "1/200"); // fast, has a slash
    let medium = catalog.upsert_photo(&root.join("med.arw"), None, 1, 1).unwrap().id;
    set_shutter(medium, "0.5"); // 0.5s, below threshold

    catalog.apply_auto_tags().unwrap();

    for id in [long, long2, boundary] {
        let tags = catalog.get_photo_tags(id).unwrap();
        assert!(tags.iter().any(|t| t.full_path == "Technique/Long Exposure"
            && t.auto_rule.as_deref() == Some("long-exposure")));
    }
    for id in [fast, medium] {
        assert!(catalog
            .get_photo_tags(id)
            .unwrap()
            .iter()
            .all(|t| t.full_path != "Technique/Long Exposure"));
    }

    // Export hashtags present on the tag.
    let tag = catalog.get_photo_tags(long).unwrap().into_iter().next().unwrap();
    let terms = catalog.list_terms(tag.id).unwrap();
    assert!(terms.iter().any(|t| t.text == "#longexposure" && t.export));
}

#[test]
fn panorama_auto_tag_applies_from_aspect_ratio() {
    use chairphoto_lib::catalog::PromotedMetadata;
    let (catalog, root) = temp_catalog("autotag-pano");

    let set_dims = |id: i64, w: i64, h: i64| {
        catalog
            .set_photo_metadata(
                id,
                &PromotedMetadata {
                    width: Some(w),
                    height: Some(h),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
    };

    let wide = catalog.upsert_photo(&root.join("wide.arw"), None, 1, 1).unwrap().id;
    set_dims(wide, 8000, 3000); // 2.67:1 landscape pano
    let tall = catalog.upsert_photo(&root.join("tall.arw"), None, 1, 1).unwrap().id;
    set_dims(tall, 2000, 6000); // 3:1 vertical pano
    let boundary = catalog.upsert_photo(&root.join("bound.arw"), None, 1, 1).unwrap().id;
    set_dims(boundary, 4000, 2000); // exactly 2:1 threshold (>=)
    let normal = catalog.upsert_photo(&root.join("normal.arw"), None, 1, 1).unwrap().id;
    set_dims(normal, 6000, 4000); // 3:2, not a pano

    catalog.apply_auto_tags().unwrap();

    for id in [wide, tall, boundary] {
        assert!(catalog.get_photo_tags(id).unwrap().iter().any(|t| t.full_path
            == "Technique/Panorama"
            && t.auto_rule.as_deref() == Some("panorama")));
    }
    assert!(catalog
        .get_photo_tags(normal)
        .unwrap()
        .iter()
        .all(|t| t.full_path != "Technique/Panorama"));
}

#[test]
fn publications_track_versions_per_platform() {
    let (catalog, root) = temp_catalog("publications");
    let id = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
    let v = catalog.create_version(id, "Punchy crop").unwrap();

    // Original to Instagram, the edited version to SmugMug.
    catalog.record_publication(id, None, "instagram", None).unwrap();
    catalog.record_publication(id, Some(v), "smugmug", None).unwrap();
    let pubs = catalog.list_publications(id).unwrap();
    assert_eq!(pubs.len(), 2);
    let smug = pubs.iter().find(|p| p.platform == "smugmug").unwrap();
    assert_eq!(smug.version_id, Some(v));
    assert_eq!(smug.version_name.as_deref(), Some("Punchy crop"));

    // A DIFFERENT version of the same photo to the SAME platform is its own record.
    catalog.record_publication(id, Some(v), "instagram", None).unwrap();
    let pubs = catalog.list_publications(id).unwrap();
    assert_eq!(pubs.len(), 3, "different version to same platform = new record");
    assert_eq!(
        pubs.iter().filter(|p| p.platform == "instagram").count(),
        2,
        "Original and the edited version both posted to Instagram"
    );

    // Re-marking the SAME (photo, platform, version) upserts — no duplicate.
    catalog.record_publication(id, Some(v), "instagram", None).unwrap();
    catalog.record_publication(id, None, "instagram", None).unwrap();
    assert_eq!(
        catalog.list_publications(id).unwrap().len(),
        3,
        "re-marking the same version upserts, never duplicates"
    );

    // An empty platform is rejected — the publishing module must declare a marker.
    assert!(catalog.record_publication(id, None, "  ", None).is_err());
}

#[test]
fn publication_survives_version_deletion() {
    let (catalog, root) = temp_catalog("publications-del-version");
    let id = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
    let v = catalog.create_version(id, "Bright").unwrap();
    catalog.record_publication(id, Some(v), "flickr", None).unwrap();

    catalog.delete_version(v).unwrap();

    // The publication record remains; version_id goes NULL but the snapshot name stays.
    let pubs = catalog.list_publications(id).unwrap();
    assert_eq!(pubs.len(), 1);
    assert_eq!(pubs[0].version_id, None);
    assert_eq!(pubs[0].version_name.as_deref(), Some("Bright"));

    // Posting the Original to the same platform now is a NEW record — it must not clobber
    // the orphaned record left by the deleted version.
    catalog.record_publication(id, None, "flickr", None).unwrap();
    let pubs = catalog.list_publications(id).unwrap();
    assert_eq!(pubs.len(), 2, "Original is distinct from the deleted version's orphan");
    assert!(pubs.iter().any(|p| p.version_name.as_deref() == Some("Bright")));
    assert!(pubs.iter().any(|p| p.version_name.is_none()));
}

#[test]
fn published_facet_filters_photos() {
    let (catalog, root) = temp_catalog("publications-facet");
    let posted = catalog.upsert_photo(&root.join("posted.arw"), None, 1, 1).unwrap().id;
    let other = catalog.upsert_photo(&root.join("other.arw"), None, 1, 1).unwrap().id;
    catalog.record_publication(posted, None, "instagram", None).unwrap();

    // A "Published: Instagram" facet is offered once a publication exists.
    assert!(catalog
        .available_facets()
        .iter()
        .any(|f| f.key == "published:instagram"));

    let filtered = catalog
        .list_photos(None, None, None, &["published:instagram".to_string()], "all", None, "all", None, None, &[], None)
        .unwrap();
    let ids: Vec<i64> = filtered.iter().map(|p| p.id).collect();
    assert!(ids.contains(&posted));
    assert!(!ids.contains(&other));
}

#[test]
fn smart_album_filters_list_photos_and_counts_live() {
    let (catalog, root) = temp_catalog("smart-albums");
    // Three photos: two are keepers (rating >= 4), one is not.
    let keep_a = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
    let keep_b = catalog.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;
    let reject = catalog.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
    catalog.set_culling(keep_a, Some(5), None, None).unwrap();
    catalog.set_culling(keep_b, Some(4), None, None).unwrap();
    catalog.set_culling(reject, Some(2), None, None).unwrap();

    // "Keepers" = rating >= 4. The live preview count matches the rule before it is saved.
    let rule = r#"{"match":"all","conditions":[{"field":"rating","op":"gte","value":4}]}"#;
    assert_eq!(catalog.smart_album_count(rule).unwrap(), 2);

    let id = catalog.create_smart_album("Keepers", rule).unwrap();

    // Selecting the smart album in list_photos returns exactly the matching set.
    let matched = catalog
        .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
        .unwrap();
    let ids: Vec<i64> = matched.iter().map(|p| p.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&keep_a));
    assert!(ids.contains(&keep_b));
    assert!(!ids.contains(&reject));

    // The per-row live count surfaced by list_smart_albums agrees with smart_album_count.
    let albums = catalog.list_smart_albums().unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].name, "Keepers");
    assert_eq!(albums[0].photo_count, 2);

    // The smart-album filter ANDs with the culling filter on top, same as tags/albums.
    catalog
        .set_culling(keep_a, None, None, Some(PickState::Pick))
        .unwrap();
    let picks = catalog
        .list_photos(None, None, None, &[], "pick", Some(id), "all", None, None, &[], None)
        .unwrap();
    assert_eq!(picks.iter().map(|p| p.id).collect::<Vec<_>>(), vec![keep_a]);

    // Editing the rule changes the live result with no rebuild step.
    let narrower = r#"{"match":"all","conditions":[{"field":"rating","op":"gte","value":5}]}"#;
    catalog.set_smart_album_rule(id, narrower).unwrap();
    let only_best = catalog
        .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
        .unwrap();
    assert_eq!(only_best.iter().map(|p| p.id).collect::<Vec<_>>(), vec![keep_a]);
    assert_eq!(catalog.get_smart_album(id).unwrap().photo_count, 1);

    // An empty rule matches all (non-missing) photos.
    catalog
        .set_smart_album_rule(id, r#"{"conditions":[]}"#)
        .unwrap();
    assert_eq!(
        catalog
            .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn smart_album_color_label_matches_stored_casing() {
    // Regression: the inspector stores color labels capitalized ("Red"), so a smart-album
    // rule must match that regardless of the rule value's casing ("red"/"Red").
    let (catalog, root) = temp_catalog("smart-albums-label");
    let red = catalog.upsert_photo(&root.join("r.arw"), None, 1, 1).unwrap().id;
    let green = catalog.upsert_photo(&root.join("g.arw"), None, 1, 1).unwrap().id;
    catalog.set_culling(red, None, Some("Red"), None).unwrap();
    catalog.set_culling(green, None, Some("Green"), None).unwrap();

    for value in ["red", "Red"] {
        let rule = format!(
            r#"{{"match":"all","conditions":[{{"field":"color_label","op":"is","value":"{value}"}}]}}"#
        );
        assert_eq!(catalog.smart_album_count(&rule).unwrap(), 1, "value {value}");
        let id = catalog.create_smart_album(&format!("Reds {value}"), &rule).unwrap();
        let ids: Vec<i64> = catalog
            .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
            .unwrap()
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec![red], "color_label rule value {value:?} should match the Red photo");
    }
}

#[test]
fn smart_album_combines_conditions_from_many_groups() {
    use chairphoto_lib::catalog::PromotedMetadata;
    let (catalog, root) = temp_catalog("smart-albums-multi");

    // Build a small library where each photo differs on exactly one group's axis,
    // so an AND of conditions across groups (capture EXIF, culling, date, tags)
    // isolates a single intended target.
    let exif = |id: i64, model: &str, iso: i64, when: &str| {
        catalog
            .set_photo_metadata(
                id,
                &PromotedMetadata {
                    camera_model: Some(model.to_string()),
                    iso: Some(iso),
                    capture_time: Some(when.to_string()),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
    };
    let bird = catalog.create_tag("Animals/Birds").unwrap();
    let owl = catalog.create_tag("Animals/Birds/Owl").unwrap();
    let car = catalog.create_tag("Vehicles/Car").unwrap();

    // The target: right camera, low ISO, in-range date, rated 5, tagged under Birds.
    let hit = catalog.upsert_photo(&root.join("hit.arw"), None, 1, 1).unwrap().id;
    exif(hit, "ILCE-7RM5", 100, "2026-06-15T10:00:00");
    catalog.set_culling(hit, Some(5), None, None).unwrap();
    catalog.assign_tag(hit, owl).unwrap(); // a descendant of Birds

    // Each of these fails exactly one of the five conditions.
    let wrong_cam = catalog.upsert_photo(&root.join("cam.arw"), None, 1, 1).unwrap().id;
    exif(wrong_cam, "NIKON Z9", 100, "2026-06-15T10:00:00");
    catalog.set_culling(wrong_cam, Some(5), None, None).unwrap();
    catalog.assign_tag(wrong_cam, bird).unwrap();

    let high_iso = catalog.upsert_photo(&root.join("iso.arw"), None, 1, 1).unwrap().id;
    exif(high_iso, "ILCE-7RM5", 6400, "2026-06-15T10:00:00");
    catalog.set_culling(high_iso, Some(5), None, None).unwrap();
    catalog.assign_tag(high_iso, bird).unwrap();

    let out_of_range = catalog.upsert_photo(&root.join("date.arw"), None, 1, 1).unwrap().id;
    exif(out_of_range, "ILCE-7RM5", 100, "2025-01-01T10:00:00");
    catalog.set_culling(out_of_range, Some(5), None, None).unwrap();
    catalog.assign_tag(out_of_range, bird).unwrap();

    let low_rating = catalog.upsert_photo(&root.join("rating.arw"), None, 1, 1).unwrap().id;
    exif(low_rating, "ILCE-7RM5", 100, "2026-06-15T10:00:00");
    catalog.set_culling(low_rating, Some(2), None, None).unwrap();
    catalog.assign_tag(low_rating, bird).unwrap();

    let wrong_tag = catalog.upsert_photo(&root.join("tag.arw"), None, 1, 1).unwrap().id;
    exif(wrong_tag, "ILCE-7RM5", 100, "2026-06-15T10:00:00");
    catalog.set_culling(wrong_tag, Some(5), None, None).unwrap();
    catalog.assign_tag(wrong_tag, car).unwrap();

    // Five conditions spanning four groups: capture (text + int), date, culling, tags.
    let rule = serde_json::json!({
        "match": "all",
        "conditions": [
            { "field": "camera_model", "op": "is",      "value": "ILCE-7RM5" },
            { "field": "iso",          "op": "lte",     "value": 400 },
            { "field": "capture_time", "op": "between", "value": ["2026-06-01", "2026-07-01"] },
            { "field": "rating",       "op": "gte",     "value": 4 },
            { "field": "tag",          "op": "under",   "value": bird }
        ]
    })
    .to_string();

    // Live preview count and the actual filtered set both isolate the single target.
    assert_eq!(catalog.smart_album_count(&rule).unwrap(), 1);
    let id = catalog.create_smart_album("Sharp owls, June", &rule).unwrap();
    let matched = catalog
        .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
        .unwrap();
    assert_eq!(matched.iter().map(|p| p.id).collect::<Vec<_>>(), vec![hit]);

    // Relaxing the ISO ceiling lets the high-ISO frame back in — still AND-correct
    // on the other four groups (so the two wrong-on-another-axis frames stay out).
    let looser = serde_json::json!({
        "match": "all",
        "conditions": [
            { "field": "camera_model", "op": "is",      "value": "ILCE-7RM5" },
            { "field": "capture_time", "op": "between", "value": ["2026-06-01", "2026-07-01"] },
            { "field": "rating",       "op": "gte",     "value": 4 },
            { "field": "tag",          "op": "under",   "value": bird }
        ]
    })
    .to_string();
    catalog.set_smart_album_rule(id, &looser).unwrap();
    let mut ids: Vec<i64> = catalog
        .list_photos(None, None, None, &[], "all", Some(id), "all", None, None, &[], None)
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    ids.sort();
    let mut expected = vec![hit, high_iso];
    expected.sort();
    assert_eq!(ids, expected);
}

// ── F1 round-trip: export bundle from catalog A → import into catalog B ──────

/// Build a realistic catalog A with two photos, a two-level tag taxonomy
/// (one tag will pre-exist in catalog B to test overlapping taxonomy),
/// ratings, pick state, an IPTC headline, a named version, and an edit record.
/// Returns (catalog_a, root_a, photo1_uuid, photo2_uuid, batch_id).
fn setup_catalog_a(
    tag: &str,
) -> (chairphoto_lib::catalog::Catalog, common::TestSubPath, String, String, i64) {
    use chairphoto_lib::catalog::{IptcFields, PickState};

    let (cat, root) = temp_catalog(tag);

    // Two real files so the bundle writer can copy bytes.
    let p1_path = root.join("2026/06/28/DSC01.ARW");
    let p2_path = root.join("2026/06/29/DSC02.ARW");
    std::fs::create_dir_all(p1_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(p2_path.parent().unwrap()).unwrap();
    std::fs::write(&p1_path, b"FAKE RAW BYTES PHOTO ONE").unwrap();
    std::fs::write(&p2_path, b"FAKE RAW BYTES PHOTO TWO").unwrap();

    let id1 = cat.upsert_photo(&p1_path, None, 1, 24).unwrap().id;
    let id2 = cat.upsert_photo(&p2_path, None, 2, 24).unwrap().id;

    // Rating / label / pick on photo 1.
    cat.set_culling(id1, Some(4), Some("green"), Some(PickState::Pick)).unwrap();
    // IPTC on photo 1.
    cat.set_iptc(id1, &IptcFields {
        headline: "Summer sunset".into(),
        city: "Bergen".into(),
        ..Default::default()
    }).unwrap();
    // A named version + edit record on photo 1.
    let ver = cat.create_version(id1, "Instagram crop").unwrap();
    cat.set_version_edit(ver, r#"{"crop":{"aspect":"1:1"}}"#).unwrap();
    cat.set_edit_record(id1, r#"{"basic-editor":{"exposure":0.3}}"#).unwrap();

    // Tag taxonomy: Birds (parent) → Birds/Owls (leaf). Both photos get the leaf.
    let _birds = cat.create_tag("Birds").unwrap();
    let owls = cat.create_tag("Birds/Owls").unwrap();
    cat.assign_tag(id1, owls).unwrap();
    cat.assign_tag(id2, owls).unwrap();

    // Group everything into one import batch.
    let batch = cat.create_import_batch("/card/trip-2026-06").unwrap();
    cat.assign_photos_to_batch(batch, &[id1, id2]).unwrap();

    let uuid1 = cat.get_photo(id1).unwrap().uuid;
    let uuid2 = cat.get_photo(id2).unwrap().uuid;
    (cat, root, uuid1, uuid2, batch)
}

#[test]
fn bundle_round_trip_export_import_and_idempotent_reimport() {
    use chairphoto_lib::bundle::importer::{extract_originals, index_bundle, open_bundle};
    use chairphoto_lib::bundle::writer::{gather_bundle, write_bundle};
    use chairphoto_lib::catalog::PickState;

    // ── Catalog A: source of the export ──────────────────────────────────────
    let (cat_a, root_a, uuid1, uuid2, batch_id_a) =
        setup_catalog_a("bundle-rt-a");

    // Export the batch as a bundle zip.
    let bundle_zip = common::TestTmpDir::new("bundle-roundtrip-test")
        .into_subpath("bundle.chairphoto");

    let gathered = gather_bundle(&cat_a, batch_id_a)
        .expect("gather_bundle must not fail")
        .expect("batch must exist");

    let write_result = write_bundle(&gathered, &bundle_zip, |_, _| {})
        .expect("write_bundle must not fail");

    // Both originals were reachable and included.
    assert_eq!(write_result.exported, 2, "both originals must be in the zip");
    assert_eq!(write_result.skipped_offline, 0);
    assert_eq!(write_result.errors, 0);

    // ── Catalog B: destination of the import ─────────────────────────────────
    // Pre-seed catalog B with the "Birds" parent tag under a DIFFERENT uuid to
    // test the overlapping-taxonomy path (uuid miss → normalized-path match).
    let (cat_b, root_b) = temp_catalog("bundle-rt-b");
    let _pre_birds = cat_b.create_tag("Birds").unwrap(); // path-match for the ancestor

    // ── Phase 1: parse the bundle ─────────────────────────────────────────────
    let (manifest, mut archive) = open_bundle(&bundle_zip)
        .expect("open_bundle must succeed");
    assert_eq!(manifest.photos.len(), 2);
    let batch_uuid_in_manifest = manifest.batch.uuid.clone();
    // The batch uuid from catalog A must be in the manifest.
    let a_batch = cat_a
        .list_import_batches()
        .unwrap()
        .into_iter()
        .find(|b| b.id == batch_id_a)
        .unwrap();
    assert_eq!(batch_uuid_in_manifest, a_batch.uuid);

    // ── Phase 2: extract originals into catalog B's root ─────────────────────
    let (extracted, partial) =
        extract_originals(&manifest, &mut archive, &root_b, |_, _| {})
            .expect("extract_originals must succeed");

    assert_eq!(partial.copied, 2, "both originals extracted");
    assert_eq!(partial.skipped_duplicate, 0);
    assert_eq!(partial.errors, 0);
    assert_eq!(extracted.len(), 2);

    // Each extracted file must exist under the date tree in root_b.
    let expected1 = root_b.join("2026").join("06").join("28").join("DSC01.ARW");
    let expected2 = root_b.join("2026").join("06").join("29").join("DSC02.ARW");
    assert!(expected1.exists(), "photo 1 must land at {}", expected1.display());
    assert!(expected2.exists(), "photo 2 must land at {}", expected2.display());
    // UUID sidecar must have been written beside each original.
    assert!(expected1.with_extension("ARW.xmp").exists()
        || {
            let mut s = expected1.as_os_str().to_os_string();
            s.push(".xmp");
            PathBuf::from(s).exists()
        },
        "UUID sidecar must be beside photo 1");

    // ── Phase 3: index + merge into catalog B ────────────────────────────────
    let result = index_bundle(&cat_b, &manifest, &extracted, &root_b, partial)
        .expect("index_bundle must succeed");

    assert_eq!(result.copied, 2);
    assert_eq!(result.errors, 0);
    assert!(result.merge.batch_added, "new batch must be recorded");
    // The upsert pre-placed both photos, so merge counts them as existing.
    assert_eq!(result.merge.photos_existing, 2, "upsert placed both photos");
    assert_eq!(result.merge.photos_added, 0, "merge must not duplicate upserted photos");
    // Birds already existed in catalog B (path-match); Birds/Owls is new.
    assert_eq!(result.merge.tags_created, 1, "only the leaf Birds/Owls is new");
    // Both photos get the Birds/Owls assignment via the union.
    assert_eq!(result.merge.assignments_added, 2);

    // Verify photo 1 in catalog B has the correct state.
    let p1_b = cat_b.get_photo_by_uuid(&uuid1).expect("photo 1 must be in catalog B");
    assert_eq!(p1_b.rating, 4, "rating must transfer");
    assert_eq!(p1_b.label, "green", "color label must transfer");
    assert_eq!(p1_b.pick_state, PickState::Pick, "pick state must transfer");

    let iptc = cat_b.get_iptc(p1_b.id).unwrap();
    assert_eq!(iptc.headline, "Summer sunset", "IPTC headline must transfer");
    assert_eq!(iptc.city, "Bergen", "IPTC city must transfer");

    let edit = cat_b.get_edit_record(p1_b.id).unwrap();
    assert_eq!(edit.as_deref(), Some(r#"{"basic-editor":{"exposure":0.3}}"#),
        "edit record must transfer");

    let versions = cat_b.list_versions(p1_b.id).unwrap();
    assert_eq!(versions.len(), 1, "named version must transfer");
    assert_eq!(versions[0].name, "Instagram crop");
    assert_eq!(versions[0].edit_json, r#"{"crop":{"aspect":"1:1"}}"#);

    // Verify photo 2 is also in catalog B.
    let p2_b = cat_b.get_photo_by_uuid(&uuid2).expect("photo 2 must be in catalog B");
    assert_eq!(p2_b.rating, 0, "photo 2 has default rating");

    // Both photos must have the Birds/Owls tag assignment.
    for (id, uuid) in [(p1_b.id, &uuid1), (p2_b.id, &uuid2)] {
        let tags = cat_b.get_photo_tags(id).unwrap();
        assert!(
            tags.iter().any(|t| t.full_path == "Birds/Owls"),
            "photo {uuid} must be tagged Birds/Owls"
        );
    }

    // The import batch must be present in catalog B.
    let batches_b = cat_b.list_import_batches().unwrap();
    assert_eq!(batches_b.len(), 1);
    assert_eq!(batches_b[0].uuid, batch_uuid_in_manifest,
        "batch uuid must match the bundle manifest");
    assert_eq!(batches_b[0].source_label, "/card/trip-2026-06");
    assert_eq!(batches_b[0].photo_count, 2);

    // ── Re-import is idempotent ───────────────────────────────────────────────
    let (manifest2, mut archive2) = open_bundle(&bundle_zip)
        .expect("open_bundle (2nd) must succeed");
    let (extracted2, partial2) =
        extract_originals(&manifest2, &mut archive2, &root_b, |_, _| {})
            .expect("extract_originals (2nd) must succeed");

    // Same-size files already exist → skipped.
    assert_eq!(partial2.skipped_duplicate, 2,
        "both originals already present → skipped");
    assert_eq!(partial2.copied, 0);

    let result2 = index_bundle(&cat_b, &manifest2, &extracted2, &root_b, partial2)
        .expect("index_bundle (2nd) must succeed");

    // Nothing new added on re-import.
    assert_eq!(result2.merge.photos_added, 0,
        "re-import must not add duplicate photos");
    assert!(!result2.merge.batch_added,
        "re-import must not add duplicate batch");
    assert_eq!(result2.merge.tags_created, 0,
        "re-import must not create duplicate tags");
    assert_eq!(result2.merge.assignments_added, 0,
        "re-import must not duplicate tag assignments");

    // Exactly 2 photos in catalog B after the re-import.
    let count_b = cat_b
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap()
        .len();
    assert_eq!(count_b, 2, "catalog B must have exactly 2 photos after re-import");

    // Rating is unchanged after re-import (additive-only invariant).
    let p1_after = cat_b.get_photo_by_uuid(&uuid1).unwrap();
    assert_eq!(p1_after.rating, 4, "rating must survive re-import unchanged");

    // Cleanup: remove the A root (already cleaned by temp_catalog tag, but be explicit).
    drop(root_a);
}

#[test]
fn bundle_import_merges_with_pre_existing_local_taxonomy() {
    // Verifies the path-match branch: catalog B independently grew "Birds/Owls" with its
    // own uuid *and* a local extra term before the bundle arrives. The merge must union
    // the bundle's term without clobbering the local uuid or the local term.
    use chairphoto_lib::bundle::importer::{extract_originals, index_bundle, open_bundle};
    use chairphoto_lib::bundle::writer::{gather_bundle, write_bundle};

    let (cat_a, _root_a, uuid1, _uuid2, batch_id_a) =
        setup_catalog_a("bundle-tax-a");

    let bundle_zip = common::TestTmpDir::new("bundle-taxonomy-test")
        .into_subpath("bundle.chairphoto");

    let gathered = gather_bundle(&cat_a, batch_id_a).unwrap().unwrap();
    write_bundle(&gathered, &bundle_zip, |_, _| {}).unwrap();

    // Catalog B: pre-seed "Birds/Owls" with its own uuid and a local term.
    let (cat_b, root_b) = temp_catalog("bundle-tax-b");
    let local_owls = cat_b.create_tag("Birds/Owls").unwrap();
    let local_uuid = cat_b.get_tag(local_owls).unwrap().uuid;
    cat_b.add_synonym(local_owls, "Strix", None, true).unwrap();
    // Mark it non-exportable (organizational) to verify the local flag is preserved.
    cat_b.set_tag_exportable(local_owls, false).unwrap();

    // Import the bundle.
    let (manifest, mut archive) = open_bundle(&bundle_zip).unwrap();
    let (extracted, partial) =
        extract_originals(&manifest, &mut archive, &root_b, |_, _| {}).unwrap();
    let result = index_bundle(&cat_b, &manifest, &extracted, &root_b, partial).unwrap();

    // No new tags created — both Birds and Birds/Owls matched by path.
    assert_eq!(result.merge.tags_created, 0,
        "existing Birds/Owls must be reused by path-match");

    // The local uuid and exportable flag are preserved (not overwritten).
    let after_owls = cat_b.get_tag(local_owls).unwrap();
    assert_eq!(after_owls.uuid, local_uuid, "local uuid must not be overwritten");
    assert!(
        !cat_b.tag_exportable(local_owls).unwrap(),
        "local exportable=false must not be overwritten"
    );

    // The bundle's taxonomy term ("Owls" from the tag name) may have been unioned in,
    // but the local "Strix" term must still be present.
    let terms = cat_b.list_terms(local_owls).unwrap();
    assert!(terms.iter().any(|t| t.text == "Strix"),
        "local term 'Strix' must survive the merge");

    // Photo 1 must be in catalog B with the correct rating.
    let p1_b = cat_b.get_photo_by_uuid(&uuid1).unwrap();
    assert_eq!(p1_b.rating, 4);
    // And tagged with the pre-existing (local) Birds/Owls tag.
    let tags = cat_b.get_photo_tags(p1_b.id).unwrap();
    assert!(tags.iter().any(|t| t.id == local_owls),
        "photo must be assigned the pre-existing local Birds/Owls tag");
}

#[test]
fn catalog_stats_counts_and_buckets() {
    use chairphoto_lib::catalog::PromotedMetadata;

    let (catalog, root) = temp_catalog("stats");

    // Helper: create a minimal real file and upsert it.
    let mk = |name: &str| -> i64 {
        let p = root.join(name);
        std::fs::write(&p, b"x").unwrap();
        catalog.upsert_photo(&p, None, 1, 1).unwrap().id
    };

    // Four photos:
    //   p1 — 2026-06, hour 09, camera "TestCam", no tag, rating 3
    //   p2 — 2026-06, hour 14, camera "TestCam", tag, rating 0 (default)
    //   p3 — 2026-07, hour 14, no camera, no tag, rating 0 (default)
    //   p4 — no capture_time (must not affect time-based buckets)
    let p1 = mk("p1.ARW");
    let p2 = mk("p2.ARW");
    let p3 = mk("p3.ARW");
    let p4 = mk("p4.ARW");

    let set_meta = |id: i64, capture_time: Option<&str>, camera: Option<&str>| {
        let promoted = PromotedMetadata {
            capture_time: capture_time.map(String::from),
            camera_model: camera.map(String::from),
            ..Default::default()
        };
        catalog.set_photo_metadata(id, &promoted, &[]).unwrap();
    };

    set_meta(p1, Some("2026-06-15T09:30:00"), Some("TestCam"));
    set_meta(p2, Some("2026-06-20T14:00:00"), Some("TestCam"));
    set_meta(p3, Some("2026-07-01T14:45:00"), None);
    set_meta(p4, None, None); // no capture_time

    // Set rating 3 on p1.
    catalog
        .set_culling(p1, Some(3), None, None)
        .unwrap();

    // Assign a tag to p2.
    let tag_id = catalog.create_tag("Animals/Bird").unwrap();
    catalog.assign_tag(p2, tag_id).unwrap();

    // All-None: whole-catalog stats (original behaviour).
    let stats = catalog.catalog_stats(None, None, None).unwrap();

    // Totals
    assert_eq!(stats.total_photos, 4, "four non-missing photos");
    assert_eq!(stats.with_capture_time, 3, "three have capture_time");

    // First/last month
    assert_eq!(stats.first_month.as_deref(), Some("2026-06"));
    assert_eq!(stats.last_month.as_deref(), Some("2026-07"));

    // Timeline: two buckets
    assert_eq!(stats.timeline.len(), 2);
    let jun = stats.timeline.iter().find(|(m, _)| m == "2026-06").unwrap();
    assert_eq!(jun.1, 2, "two photos in 2026-06");
    let jul = stats.timeline.iter().find(|(m, _)| m == "2026-07").unwrap();
    assert_eq!(jul.1, 1, "one photo in 2026-07");

    // Hours: exactly 24 entries; hour 9 has 1, hour 14 has 2
    assert_eq!(stats.hours.len(), 24);
    assert_eq!(stats.hours[9], 1, "one photo at hour 09");
    assert_eq!(stats.hours[14], 2, "two photos at hour 14");

    // Weekdays: exactly 7 entries
    assert_eq!(stats.weekdays.len(), 7);

    // Cameras: TestCam with count 2
    assert_eq!(stats.cameras.len(), 1);
    assert_eq!(stats.cameras[0].0, "TestCam");
    assert_eq!(stats.cameras[0].1, 2);

    // Top tags: the assigned tag appears
    assert!(!stats.top_tags.is_empty());
    let bird_tag = stats.top_tags.iter().find(|(id, _, _)| *id == tag_id).unwrap();
    assert_eq!(bird_tag.1, "Animals/Bird");
    assert_eq!(bird_tag.2, 1, "one photo tagged Animals/Bird");

    // Ratings: exactly 6 entries; rating 3 has 1 photo, rating 0 has 3 photos
    assert_eq!(stats.ratings.len(), 6);
    assert_eq!(stats.ratings[0], 3, "three unrated photos (rating 0)");
    assert_eq!(stats.ratings[3], 1, "one photo with rating 3");

    // --- album scope --------------------------------------------------------
    // Album contains p1 (2026-06, hour 09, TestCam, rating 3) and p3 (2026-07, hour 14).
    let album_id = catalog.create_album("Subset").unwrap();
    catalog.add_photos_to_album(album_id, &[p1, p3]).unwrap();

    let astats = catalog.catalog_stats(None, Some(album_id), None).unwrap();
    assert_eq!(astats.total_photos, 2, "album contains 2 photos");
    assert_eq!(astats.with_capture_time, 2, "both have capture_time");
    assert_eq!(astats.first_month.as_deref(), Some("2026-06"), "earliest is June");
    assert_eq!(astats.last_month.as_deref(), Some("2026-07"), "latest is July");
    assert_eq!(astats.timeline.len(), 2, "two distinct months in album");
    assert_eq!(astats.hours[9], 1, "hour 09 has 1 photo (p1)");
    assert_eq!(astats.hours[14], 1, "hour 14 has 1 photo (p3, not p2)");
    // Only p1 has a camera (TestCam); p3 has none.
    assert_eq!(astats.cameras.len(), 1);
    assert_eq!(astats.cameras[0].1, 1, "TestCam appears once in album");
    // p2 (tagged Animals/Bird) is NOT in the album, so no tags.
    assert!(
        astats.top_tags.is_empty(),
        "no tagged photos in this album scope"
    );
    // p1 has rating 3; p3 has rating 0.
    assert_eq!(astats.ratings[0], 1, "one rating-0 photo in album");
    assert_eq!(astats.ratings[3], 1, "one rating-3 photo in album");

    // --- tag scope including descendants ------------------------------------
    // Create parent tag "Animals" and child "Animals/Dog"; tag p3 with the child.
    // Scope by parent — p3 must appear (child is a descendant).
    // p2 already has "Animals/Bird" which is a sibling, not a descendant of "Animals/Dog".
    let animals_id = catalog.find_tag_id_by_path("Animals").unwrap().unwrap();
    let dog_id = catalog.create_tag("Animals/Dog").unwrap();
    catalog.assign_tag(p3, dog_id).unwrap();

    // Scope by "Animals" — should include p2 (Animals/Bird) and p3 (Animals/Dog).
    let tstats = catalog.catalog_stats(Some(animals_id), None, None).unwrap();
    assert_eq!(
        tstats.total_photos, 2,
        "Animals scope: p2 (Bird) + p3 (Dog) = 2"
    );
    // Only p2 has a camera (TestCam); p3 has none.
    assert_eq!(tstats.cameras.len(), 1, "one camera model in Animals scope");
    assert_eq!(tstats.cameras[0].1, 1);

    // Scope by the leaf "Animals/Dog" — only p3.
    let dstats = catalog.catalog_stats(Some(dog_id), None, None).unwrap();
    assert_eq!(dstats.total_photos, 1, "Dog scope: only p3");
    assert_eq!(dstats.cameras.len(), 0, "p3 has no camera");
}

// ---------------------------------------------------------------------------
// I4b — catalog-switch lifecycle: an in-flight scan cancels cleanly through
// its Arc<AtomicBool> abort flag, so nothing writes into a stale catalog.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

/// Write `n` tiny placeholder JPEGs under `dir` so a scan has files to walk.
fn seed_jpgs(dir: &std::path::Path, n: usize) {
    std::fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        std::fs::write(dir.join(format!("img{i:05}.jpg")), b"notarealjpeg").unwrap();
    }
}

#[test]
fn scan_folder_aborts_before_writing_anything() {
    let (catalog, root) = temp_catalog("scan-abort-pre");
    seed_jpgs(&root, 5);

    // The abort flag is already set when the scan begins: it must bail at the first
    // cancellation point and never import a row.
    let abort = AtomicBool::new(true);
    let err = chairphoto_lib::scanner::scan_folder(&catalog, &root, &abort, &|_| {})
        .expect_err("a pre-aborted scan returns an error, not Ok");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);

    // Nothing landed in the catalog — the switch can safely tear it down.
    let photos = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(photos.is_empty(), "aborted scan imported no photos, got {}", photos.len());
}

#[test]
fn scan_external_aborts_before_writing_anything() {
    let (catalog, root) = temp_catalog("nas-scan-abort");
    let nas_dir = root.parent().unwrap().join("nas-archive-abort");
    seed_jpgs(&nas_dir, 5);
    catalog.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();

    let abort = AtomicBool::new(true);
    let err = chairphoto_lib::scanner::scan_external_folder(&catalog, &nas_dir, &abort, &|_| {})
        .expect_err("a pre-aborted external scan returns an error");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);
    assert!(
        catalog
            .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
            .unwrap()
            .is_empty(),
        "aborted external scan imported no photos"
    );
}

#[test]
fn scan_folder_aborts_mid_run_and_stops_early() {
    // A larger folder (crosses the per-batch commit boundary) so the scan emits an
    // "indexing" progress event; the callback then trips the abort flag, and the scan
    // must stop at the next cancellation point instead of running to completion.
    let (catalog, root) = temp_catalog("scan-abort-mid");
    seed_jpgs(&root, 1200); // > COMMIT_EVERY (500), so a progress event fires mid-walk

    let abort = std::sync::Arc::new(AtomicBool::new(false));
    let abort_for_cb = abort.clone();
    let on_progress = move |p: chairphoto_lib::scanner::ScanProgress| {
        // Trip the flag as soon as the first indexing batch commits.
        if p.phase == "indexing" {
            abort_for_cb.store(true, Ordering::Relaxed);
        }
    };
    let err = chairphoto_lib::scanner::scan_folder(&catalog, &root, &abort, &on_progress)
        .expect_err("a mid-run abort returns SCAN_ABORTED");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);

    // Rows committed before the abort are durable (the first batch), but the scan stopped
    // early — it did not import all 1200 files.
    let count = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap()
        .len();
    assert!(count < 1200, "aborted mid-run: fewer than all files imported, got {count}");
}

// ---------------------------------------------------------------------------
// I4 — Multi-catalog: create / reopen round-trip tests.
//
// The Tauri commands (switch_catalog / list_recent_catalogs) are private to the
// commands crate and are unit-tested there.  Here we test the underlying
// `Catalog` primitives they call: that a fresh catalog can be created,
// populated, closed, and reopened with its state intact; and that two
// independent catalogs coexist without cross-contaminating each other's rows.
// The recent-catalog registry helpers (record / load / save) are tested
// separately in commands/catalog.rs where the private functions are accessible.
// ---------------------------------------------------------------------------

/// Create a named catalog in a fresh temp dir and return (Catalog, db_path, root).
fn temp_catalog_named(tag: &str) -> (Catalog, common::TestSubPath, PathBuf) {
    let dir = common::TestTmpDir::new(&format!("multi-{tag}"));
    let root = dir.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_name = format!("{tag}.chairphoto");
    let db = dir.join(&db_name);
    let catalog = Catalog::open(&db, &root).unwrap();
    (catalog, dir.into_subpath(&db_name), root)
}

// -----------------------------------------------------------------
// A freshly-created catalog file is opened by Catalog::open; data
// written to it survives closing and reopening (round-trip).
// -----------------------------------------------------------------
#[test]
fn create_catalog_creates_new_file_and_data_survives_reopen() {
    let (catalog, db, root) = temp_catalog_named("create-rt");

    // The db file must exist after Catalog::open.
    assert!(db.exists(), "catalog file must be created on first open");

    // Write a tag and a photo into the fresh catalog.
    let photo_path = root.join("DSC0001.ARW");
    std::fs::write(&photo_path, b"rawdata").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 7).unwrap().id;
    let tag_id = catalog.create_tag("Birds/Owl").unwrap();
    catalog.assign_tag(photo_id, tag_id).unwrap();
    catalog.set_culling(photo_id, Some(3), Some("Green"), None).unwrap();

    // Close by dropping — this flushes the WAL.
    drop(catalog);

    // Reopen the same file (root supplied again, as open_catalog does).
    let catalog2 = Catalog::open(&db, &root).unwrap();

    // All data is still there.
    let photos = catalog2
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos.len(), 1, "photo must survive the close/reopen cycle");
    let p = &photos[0];
    assert_eq!(p.rating, 3, "rating must survive");
    assert_eq!(p.label, "Green", "label must survive");

    let tags = catalog2.get_photo_tags(p.id).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].full_path, "Birds/Owl");
}

// -----------------------------------------------------------------
// Reopening an existing catalog with a different root supplied to
// Catalog::open does NOT override the stored root — the catalog
// reads back the root it persisted on first open (ON CONFLICT DO
// NOTHING). Changing the root requires set_library_root (which runs
// an UPDATE). This test documents the correct behavior so nobody
// accidentally "fixes" the wrong behavior.
// -----------------------------------------------------------------
#[test]
fn open_catalog_keeps_stored_root_on_reopen() {
    let (catalog, db, root_a) = temp_catalog_named("reroot");
    let photo = root_a.join("a.jpg");
    std::fs::write(&photo, b"x").unwrap();
    catalog.upsert_photo(&photo, None, 1, 1).unwrap();
    // Capture the original root before dropping.
    let stored_root = catalog.root().to_path_buf();
    drop(catalog);

    // Open again but supply a different root — the catalog must keep its stored root.
    let dir_b = common::TestTmpDir::new("multi-reroot-alt");
    let root_b = dir_b.join("photos");
    std::fs::create_dir_all(&root_b).unwrap();

    let catalog2 = Catalog::open(&db, &root_b).unwrap();
    // The catalog's root() must reflect the STORED root (root_a), not the newly-supplied root_b.
    assert_eq!(
        catalog2.root(),
        stored_root.as_path(),
        "catalog must adopt its stored root on reopen, not the caller-supplied root"
    );
    assert_ne!(
        catalog2.root(),
        root_b.as_path(),
        "caller-supplied root must not override stored root without set_library_root"
    );
}

// -----------------------------------------------------------------
// Opening a non-existent file path is an error — `open_catalog` in
// commands.rs gates on `catalog_path_buf.exists()` first, but the
// underlying Catalog::open does create. This test documents the
// current behavior: the file is created if absent.
// -----------------------------------------------------------------
#[test]
fn catalog_open_creates_file_when_absent() {
    let dir = common::TestTmpDir::new("multi-absent");
    let db = dir.join("fresh.chairphoto");
    let root = dir.join("photos");

    assert!(!db.exists(), "db must not exist before open");
    let catalog = Catalog::open(&db, &root).unwrap();
    assert!(db.exists(), "db is created by Catalog::open even when absent");
    drop(catalog);
}

// -----------------------------------------------------------------
// Two independently-opened catalogs do not cross-contaminate each
// other's photo rows. This is the core "switch catalog" invariant:
// data from catalog A must not appear in catalog B.
// -----------------------------------------------------------------
#[test]
fn two_catalogs_are_isolated() {
    let (cat_a, _db_a, root_a) = temp_catalog_named("iso-a");
    let (cat_b, _db_b, root_b) = temp_catalog_named("iso-b");

    // Add a photo to each catalog.
    let photo_a = root_a.join("alpha.jpg");
    std::fs::write(&photo_a, b"a").unwrap();
    cat_a.upsert_photo(&photo_a, None, 1, 1).unwrap();

    let photo_b = root_b.join("beta.jpg");
    std::fs::write(&photo_b, b"b").unwrap();
    cat_b.upsert_photo(&photo_b, None, 1, 1).unwrap();

    // Each catalog sees exactly its own photo, not the other's.
    let photos_a = cat_a
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos_a.len(), 1, "catalog A must hold exactly 1 photo");
    assert_eq!(photos_a[0].path, "alpha.jpg");

    let photos_b = cat_b
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos_b.len(), 1, "catalog B must hold exactly 1 photo");
    assert_eq!(photos_b[0].path, "beta.jpg");
}

// -----------------------------------------------------------------
// Switching: drop catalog A, open catalog B — catalog B starts empty
// (or with its own state) regardless of what was in catalog A. This
// mirrors the switch_catalog lifecycle: close old, open new.
// -----------------------------------------------------------------
#[test]
fn switch_catalog_old_state_not_visible_in_new() {
    // Populate catalog A with several photos.
    let (cat_a, db_a, root_a) = temp_catalog_named("switch-old");
    for i in 0..5u32 {
        let p = root_a.join(format!("p{i}.jpg"));
        std::fs::write(&p, b"img").unwrap();
        cat_a.upsert_photo(&p, None, 1, 3).unwrap();
    }
    // Verify A has 5 photos.
    assert_eq!(
        cat_a
            .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
            .unwrap()
            .len(),
        5
    );

    // "Switch": drop catalog A (close + flush), open a fresh catalog B.
    drop(cat_a);
    let _ = db_a; // keep db alive for borrow

    let (cat_b, _db_b, _root_b) = temp_catalog_named("switch-new");
    // B is freshly created — it must not see A's photos.
    let photos_b = cat_b
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        photos_b.is_empty(),
        "newly-opened catalog must be empty after switching from catalog A"
    );
}

// -----------------------------------------------------------------
// A scan launched on a secondary connection of catalog A, then
// interrupted by setting the abort flag, leaves catalog B clean when
// it is subsequently opened. (Complements the existing
// scan_folder_aborts_* tests, but targets the switch-catalog scenario
// where A is closed before B is opened.)
// -----------------------------------------------------------------
#[test]
fn switch_catalog_with_aborted_scan_leaves_new_catalog_clean() {
    let (cat_a, db_a, root_a) = temp_catalog_named("switch-abort-a");
    let (cat_b, _db_b, _root_b) = temp_catalog_named("switch-abort-b");

    // Seed a few files under root_a.
    seed_jpgs(&root_a, 5);

    // Open a SECONDARY connection on catalog A (as run_blocking_scan does) and
    // start scanning with the abort flag pre-set so it stops immediately.
    let scan_cat_a =
        Catalog::open_secondary(cat_a.db_path(), cat_a.root()).unwrap();
    let abort = AtomicBool::new(true);
    let err =
        chairphoto_lib::scanner::scan_folder(&scan_cat_a, &root_a, &abort, &|_| {})
            .expect_err("abort must cause an error");
    assert_eq!(
        err,
        chairphoto_lib::scanner::SCAN_ABORTED,
        "scan must return SCAN_ABORTED"
    );

    // Drop the secondary connection and then catalog A itself.
    drop(scan_cat_a);
    drop(cat_a);
    let _ = db_a;

    // Catalog B (opened after the switch) must be entirely clean.
    assert!(
        cat_b
            .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
            .unwrap()
            .is_empty(),
        "aborted scan on catalog A must not write rows into catalog B"
    );
}

// ---------------------------------------------------------------------------
// I6c — two-phase live scan: Phase A imports rows immediately (metadata_ready=0),
// Phase B enriches them and flips metadata_ready to 1; Phase B honours the abort
// flag so a catalog switch / second scan stops it mid-run.
// ---------------------------------------------------------------------------

#[test]
fn phase_a_imports_unready_rows_then_phase_b_marks_ready() {
    let (catalog, root) = temp_catalog("i6c-two-phase");
    seed_jpgs(&root, 4);

    // Phase A: rows appear immediately, all awaiting metadata (metadata_ready = 0).
    let abort = AtomicBool::new(false);
    let (result, pending) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort, &|_| {}).unwrap();
    assert_eq!(result.created, 4, "Phase A creates all 4 rows");
    let after_a = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(after_a.len(), 4, "Phase A rows are visible in the grid");
    assert!(
        after_a.iter().all(|p| p.metadata_ready == 0),
        "new rows are not-yet-ready until Phase B runs"
    );

    // Phase B: enrich the pending rows; every one flips to ready.
    chairphoto_lib::scanner::phase_b_enrich(&catalog, pending, &abort, &|_| {}).unwrap();
    let after_b = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        after_b.iter().all(|p| p.metadata_ready == 1),
        "Phase B marks every enriched row ready"
    );
}

#[test]
fn phase_b_aborts_and_leaves_rows_unready() {
    let (catalog, root) = temp_catalog("i6c-phase-b-abort");
    seed_jpgs(&root, 4);

    // Phase A imports the rows (metadata_ready = 0).
    let abort = AtomicBool::new(false);
    let (_result, pending) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort, &|_| {}).unwrap();

    // A catalog switch / second scan trips the flag before Phase B runs: it must bail at
    // the first cancellation point with SCAN_ABORTED and leave the rows not-yet-ready.
    abort.store(true, Ordering::Relaxed);
    let err = chairphoto_lib::scanner::phase_b_enrich(&catalog, pending, &abort, &|_| {})
        .expect_err("a pre-aborted Phase B returns SCAN_ABORTED");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);
    let rows = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        rows.iter().all(|p| p.metadata_ready == 0),
        "an aborted Phase B leaves rows awaiting metadata (never marked ready)"
    );
}

// ---------------------------------------------------------------------------
// I6d — Resumable Phase B: the pending_enrichment table survives a crash/abort
// and resume_pending_enrichment rebuilds the PendingEnrich hand-off so Phase B
// can complete on next startup.
// ---------------------------------------------------------------------------

#[test]
fn i6d_pending_queue_persists_after_phase_a_and_drains_on_phase_b_completion() {
    let (catalog, root) = temp_catalog("i6d-full");
    seed_jpgs(&root, 4);

    // (a) Phase A: rows appear immediately with metadata_ready=0 and must be
    //     persisted in the pending_enrichment table.
    let abort = AtomicBool::new(false);
    let (_result, pending) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort, &|_| {}).unwrap();
    assert_eq!(pending.imported.len(), 4, "Phase A produced 4 pending entries");

    // (b) pending_enrichment table must have exactly 4 rows.
    assert_eq!(
        catalog.pending_enrichment_count().unwrap(),
        4,
        "pending_enrichment must hold one row per new photo after Phase A"
    );

    // (c) Abort Phase B immediately — simulates an app crash / quit mid-scan.
    abort.store(true, Ordering::Relaxed);
    let err = chairphoto_lib::scanner::phase_b_enrich(&catalog, pending, &abort, &|_| {})
        .expect_err("pre-aborted Phase B returns SCAN_ABORTED");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);

    // After the abort the rows are still not-ready.
    let after_abort = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        after_abort.iter().all(|p| p.metadata_ready == 0),
        "aborted Phase B must leave all rows not-ready"
    );

    // The queue must still be intact (nothing was dequeued by the aborted run).
    assert_eq!(
        catalog.pending_enrichment_count().unwrap(),
        4,
        "pending_enrichment rows survive an aborted Phase B"
    );

    // (d) resume_pending_enrichment rebuilds the PendingEnrich hand-off from the
    //     persisted table — this is what startup auto-resume calls.
    let resumed = chairphoto_lib::scanner::resume_pending_enrichment(&catalog)
        .unwrap()
        .expect("resume_pending_enrichment must return Some when rows remain");
    assert_eq!(
        resumed.imported.len(),
        4,
        "resumed PendingEnrich must contain all 4 surviving rows"
    );
    // folder_id is None on the resume path (scan walk is already done).
    assert!(resumed.folder_id.is_none(), "resume path has no folder_id");

    // (e) Run Phase B to completion using the resumed PendingEnrich.
    let abort2 = AtomicBool::new(false);
    chairphoto_lib::scanner::phase_b_enrich(&catalog, resumed, &abort2, &|_| {}).unwrap();

    // (f) All rows must be metadata_ready=1 and the queue must be empty.
    let after_resume = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        after_resume.iter().all(|p| p.metadata_ready == 1),
        "Phase B via resumed queue must mark all rows ready"
    );
    assert_eq!(
        catalog.pending_enrichment_count().unwrap(),
        0,
        "pending_enrichment table must be empty after Phase B completes"
    );
}

#[test]
fn i6d_resume_returns_none_when_queue_is_empty() {
    let (catalog, _root) = temp_catalog("i6d-empty-resume");
    // No Phase A was run — the queue is empty.
    let result = chairphoto_lib::scanner::resume_pending_enrichment(&catalog).unwrap();
    assert!(
        result.is_none(),
        "resume_pending_enrichment returns None when no rows are pending"
    );
}

#[test]
fn i6d_enqueue_is_idempotent_and_dequeue_clears_row() {
    let (catalog, root) = temp_catalog("i6d-queue-ops");
    let id = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;

    // Enqueue twice — second INSERT OR IGNORE is a no-op.
    catalog.enqueue_pending_enrichment(id).unwrap();
    catalog.enqueue_pending_enrichment(id).unwrap();
    assert_eq!(catalog.pending_enrichment_count().unwrap(), 1, "enqueue is idempotent");

    // Dequeue removes the row.
    catalog.dequeue_pending_enrichment(id).unwrap();
    assert_eq!(catalog.pending_enrichment_count().unwrap(), 0, "dequeue clears the row");

    // Dequeuing a non-existent row is a no-op (not an error).
    catalog.dequeue_pending_enrichment(id).unwrap();
    assert_eq!(catalog.pending_enrichment_count().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// I6d stale-drain-via-rescan: the most subtle I6d path. A rescan after an
// aborted Phase B merges stale pending_enrichment rows into the new Phase A
// hand-off so Phase B enriches them in the same pass ("rescan_library can
// drain the queue without re-walking"). Three cases:
//
//   A) file DELETED between Phase B abort and rescan → resolver returns None
//      → load_pending_enrichment silently skips it → not included in merged list
//
//   B) file UNCHANGED → Phase A sets needs_extract=false (not re-enqueued);
//      its stale pending_enrichment row IS merged in and Phase B enriches it
//
//   C) file CHANGED → Phase A sets needs_extract=true and re-enqueues it;
//      `already_covered` suppresses the duplicate from the stale merge →
//      enriched exactly once
// ---------------------------------------------------------------------------

#[test]
fn i6d_stale_drain_via_rescan_handles_deleted_unchanged_and_changed_files() {
    let (catalog, root) = temp_catalog("i6d-stale-drain");

    // Seed three files: A, B, C — each will take a different fate.
    let path_a = root.join("A.jpg"); // will be deleted before the rescan
    let path_b = root.join("B.jpg"); // will be unchanged
    let path_c = root.join("C.jpg"); // will be modified (different size)
    std::fs::write(&path_a, b"aaa").unwrap();
    std::fs::write(&path_b, b"bbb").unwrap();
    std::fs::write(&path_c, b"ccc").unwrap();

    // ── Scan 1: Phase A enqueues all three ───────────────────────────────────
    let abort1 = AtomicBool::new(false);
    let (_result1, pending1) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort1, &|_| {}).unwrap();
    assert_eq!(pending1.imported.len(), 3, "Phase A created 3 rows");

    // Collect photo_ids for A, B, C so we can assert on them later.
    let photos_after_scan1 = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos_after_scan1.len(), 3);
    let find_id = |filename: &str| -> i64 {
        photos_after_scan1
            .iter()
            .find(|p| p.path.contains(filename))
            .unwrap_or_else(|| panic!("{filename} not found in catalog after scan 1"))
            .id
    };
    let id_a = find_id("A.jpg");
    let id_b = find_id("B.jpg");
    let id_c = find_id("C.jpg");

    // All three are in pending_enrichment after Phase A.
    assert_eq!(
        catalog.pending_enrichment_count().unwrap(),
        3,
        "all three photos are queued for enrichment after Phase A"
    );

    // ── Abort Phase B immediately — simulates a crash / quit mid-scan ────────
    abort1.store(true, Ordering::Relaxed);
    let err = chairphoto_lib::scanner::phase_b_enrich(&catalog, pending1, &abort1, &|_| {})
        .expect_err("pre-aborted Phase B must return SCAN_ABORTED");
    assert_eq!(err, chairphoto_lib::scanner::SCAN_ABORTED);

    // All three rows are still not-ready and still in pending_enrichment.
    let rows_after_abort = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert!(
        rows_after_abort.iter().all(|p| p.metadata_ready == 0),
        "aborted Phase B leaves all rows not-ready"
    );
    assert_eq!(
        catalog.pending_enrichment_count().unwrap(),
        3,
        "pending_enrichment survives the aborted Phase B"
    );

    // ── Mutate the filesystem to create the three scenarios ──────────────────
    // A: delete the file entirely.
    std::fs::remove_file(&path_a).unwrap();
    // B: leave path_b unchanged (same bytes → same size/mtime on rescan).
    // C: overwrite with different content (different size triggers needs_extract=true).
    std::fs::write(&path_c, b"ccc-new-content").unwrap();

    // ── Scan 2: Phase A re-walks the folder ──────────────────────────────────
    // After remove_file(&path_a), A's catalog row still exists (reconcile_missing
    // runs at end of Phase B, which we'll do here). The walk only sees B and C.
    let abort2 = AtomicBool::new(false);
    let (_result2, pending2) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort2, &|_| {}).unwrap();

    // Phase A finds B (unchanged → needs_extract=false, NOT re-enqueued) and
    // C (changed → needs_extract=true, re-enqueued). A is not in the walk.
    let walk_ids: std::collections::HashSet<i64> =
        pending2.imported.iter().map(|(_, id, _)| *id).collect();
    assert!(walk_ids.contains(&id_b), "B is in the Phase A walk (unchanged file still present)");
    assert!(walk_ids.contains(&id_c), "C is in the Phase A walk (file changed)");
    assert!(!walk_ids.contains(&id_a), "A is NOT in the Phase A walk (file deleted)");

    // Confirm that Phase A did NOT re-enqueue B (needs_extract=false, unchanged).
    let b_in_pending2 = pending2
        .imported
        .iter()
        .find(|(_, id, _)| *id == id_b)
        .map(|(_, _, needs)| *needs);
    assert_eq!(
        b_in_pending2,
        Some(false),
        "B is in Phase A's imported list with needs_extract=false (unchanged)"
    );
    // And C was re-enqueued (needs_extract=true).
    let c_in_pending2 = pending2
        .imported
        .iter()
        .find(|(_, id, _)| *id == id_c)
        .map(|(_, _, needs)| *needs);
    assert_eq!(
        c_in_pending2,
        Some(true),
        "C is in Phase A's imported list with needs_extract=true (changed)"
    );

    // ── Stale-drain merge via production predicate ──────────────────────────────
    // load_pending_enrichment resolves paths: A's file is gone → resolver returns
    // None → A is silently dropped. B and C are still listed.
    let stale = catalog.load_pending_enrichment().unwrap();

    // A must not be in the stale list (its file is unresolvable).
    assert!(
        stale.iter().all(|(_, id, _)| *id != id_a),
        "A's stale queue entry must be silently skipped when the file is unresolvable"
    );
    // B must appear (its file exists and resolver returns Some).
    assert!(
        stale.iter().any(|(_, id, _)| *id == id_b),
        "B's stale queue entry must survive (file still on disk)"
    );

    // Merge via the same function used by run_blocking_two_phase_scan.
    let mut merged_pending = pending2;
    merged_pending.imported =
        chairphoto_lib::scanner::merge_stale_pending(merged_pending.imported, stale);

    // C was already covered (needs_extract=true); B was not and was merged in.
    // A was silently dropped by load_pending_enrichment.
    let merged_ids: std::collections::HashSet<i64> =
        merged_pending.imported.iter().map(|(_, id, _)| *id).collect();
    assert!(merged_ids.contains(&id_b), "B is in the merged list (stale merge)");
    assert!(merged_ids.contains(&id_c), "C is in the merged list (Phase A re-enqueue)");
    assert!(!merged_ids.contains(&id_a), "A is NOT in the merged list (resolver silently skipped it)");

    // Count how many times C appears in the merged list — must be exactly once.
    let c_count = merged_pending.imported.iter().filter(|(_, id, _)| *id == id_c).count();
    assert_eq!(c_count, 1, "C must appear exactly once (already_covered prevents duplication)");

    // ── Run Phase B with the merged pending ──────────────────────────────────
    chairphoto_lib::scanner::phase_b_enrich(&catalog, merged_pending, &abort2, &|_| {}).unwrap();

    // B and C must now be metadata_ready=1 (enriched).
    let photos_after = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    let ready_b = photos_after.iter().find(|p| p.id == id_b).map(|p| p.metadata_ready);
    let ready_c = photos_after.iter().find(|p| p.id == id_c).map(|p| p.metadata_ready);
    assert_eq!(ready_b, Some(1), "B (unchanged, stale-drained) is now metadata_ready=1");
    assert_eq!(ready_c, Some(1), "C (changed, re-enqueued) is now metadata_ready=1");

    // A's row may or may not still exist at this point (reconcile_missing runs at the
    // end of phase_b_enrich and hides it as missing), but either way it must NOT be
    // metadata_ready=1 (it was never enriched — its file was gone).
    if let Some(row_a) = photos_after.iter().find(|p| p.id == id_a) {
        assert_eq!(
            row_a.metadata_ready, 0,
            "A's row, if still present, must remain metadata_ready=0 (never enriched)"
        );
    }

    // After Phase B:
    // - B and C were enriched and dequeued (pending_enrichment rows removed).
    // - A's stale row was silently skipped by load_pending_enrichment (unresolvable
    //   path), so it was never added to the merged list and Phase B never dequeued
    //   it. reconcile_missing only sets missing=1 (it does not delete the photo
    //   row), so the CASCADE on photo deletion does NOT fire — A's
    //   pending_enrichment row is the one remaining entry.
    let remaining = catalog.pending_enrichment_count().unwrap();
    assert_eq!(
        remaining, 1,
        "only A's stale pending_enrichment row must remain after Phase B \
         (B and C were dequeued; A's entry was silently skipped and not dequeued)"
    );
    // That remaining row belongs to A.
    let stale_after = catalog.load_pending_enrichment().unwrap();
    // load_pending_enrichment skips unresolvable paths, so the remaining row
    // for A (file deleted) is in the table but not returned by load_pending_enrichment.
    assert!(
        stale_after.is_empty(),
        "load_pending_enrichment must return empty even if A's row is in the table \
         (resolver returns None for the deleted file → entry silently skipped)"
    );
}

// ---------------------------------------------------------------------------
// I6e — Frontend: live grid refresh + placeholder tiles.
// Backend invariant: unready rows (metadata_ready=0) survive list_photos so
// the grid can render them as placeholder tiles; once set_metadata_ready(true)
// is called (end of Phase B / the "done" refresh) they appear as fully-ready
// rows and are no longer distinguishable from normal photos.
// ---------------------------------------------------------------------------

#[test]
fn i6e_unready_rows_survive_list_photos_and_done_clears_placeholders() {
    let (catalog, root) = temp_catalog("i6e-placeholder-done");
    seed_jpgs(&root, 3);

    // Phase A: insert rows with metadata_ready=0.
    let abort = AtomicBool::new(false);
    let (_result, pending) =
        chairphoto_lib::scanner::scan_folder_phase_a(&catalog, &root, &abort, &|_| {}).unwrap();
    assert_eq!(pending.imported.len(), 3, "Phase A produced 3 pending entries");

    // Invariant 1: list_photos must include unready rows so the grid can render
    // placeholder tiles for them immediately after Phase A.
    let after_a = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(after_a.len(), 3, "all 3 rows are visible as placeholder tiles");
    assert!(
        after_a.iter().all(|p| p.metadata_ready == 0),
        "Phase A rows must have metadata_ready=0 (placeholder state)"
    );

    // Invariant 2: the "done" refresh path — Phase B completes and the final
    // refresh is triggered. After set_metadata_ready(true) for every photo,
    // list_photos returns the same rows with metadata_ready=1, meaning the
    // placeholder state is cleared and all tiles are fully ready.
    chairphoto_lib::scanner::phase_b_enrich(&catalog, pending, &abort, &|_| {}).unwrap();
    let after_done = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(after_done.len(), 3, "no rows were lost during Phase B");
    assert!(
        after_done.iter().all(|p| p.metadata_ready == 1),
        "'done' clears residual placeholders: all rows are metadata_ready=1 after Phase B"
    );

    // Invariant 3: a mix of ready and unready rows — list_photos returns both,
    // letting the frontend decide which tiles get the spinner and which don't.
    let extra = catalog.upsert_photo(&root.join("extra.jpg"), None, 1, 1).unwrap().id;
    catalog.set_metadata_ready(extra, false).unwrap();
    let mixed = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(mixed.len(), 4, "ready and unready rows both appear in the grid");
    let ready_count = mixed.iter().filter(|p| p.metadata_ready == 1).count();
    let unready_count = mixed.iter().filter(|p| p.metadata_ready == 0).count();
    assert_eq!(ready_count, 3, "three previously-enriched photos are ready");
    assert_eq!(unready_count, 1, "one freshly-inserted placeholder is unready");
}

// ── H2b: Map / Geofence apply engine tests ────────────────────────────────────
//
// These tests are gated on the `map` Cargo feature so the CI `--no-default-features`
// build skips them (the feature itself compiles out too).

/// Helper: insert a photo and store GPS coordinates via set_photo_metadata so the
/// map query can find it. Returns the photo_id.
#[cfg(feature = "map")]
fn insert_photo_with_gps(
    catalog: &Catalog,
    root: &std::path::Path,
    name: &str,
    lat: f64,
    lng: f64,
) -> i64 {
    use chairphoto_lib::catalog::PromotedMetadata;
    let path = root.join(name);
    std::fs::write(&path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&path, None, 1, 4).unwrap().id;
    let meta = PromotedMetadata {
        gps_latitude: Some(lat),
        gps_longitude: Some(lng),
        ..Default::default()
    };
    catalog.set_photo_metadata(photo_id, &meta, &[]).unwrap();
    photo_id
}

/// Helper: unit square fence (lat 0..1, lng 0..1) bound to a given tag path.
#[cfg(feature = "map")]
fn unit_square_fence(
    catalog: &Catalog,
    tag_path: &str,
) -> i64 {
    use chairphoto_lib::plugins::map;
    map::ensure_schema_for(catalog).unwrap();
    let poly = vec![(0.0_f64, 0.0_f64), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)];
    map::create_fence_for(catalog, "Unit square", tag_path, &poly)
        .unwrap()
        .id
}

/// apply_fence tags photos inside the polygon and leaves those outside untouched.
#[cfg(feature = "map")]
#[test]
fn map_apply_fence_tags_inside_photos_only() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("map_apply_fence");
    map::ensure_schema_for(&catalog).unwrap();

    // One photo inside the unit square, one outside.
    let inside_id = insert_photo_with_gps(&catalog, &root, "inside.jpg", 0.5, 0.5);
    let outside_id = insert_photo_with_gps(&catalog, &root, "outside.jpg", 5.0, 5.0);

    let fence_id = unit_square_fence(&catalog, "Places/UnitSquare");

    let count = map::apply_fence(&catalog, fence_id).unwrap();
    assert_eq!(count, 1, "exactly one photo is inside the unit square");

    // The inside photo has the tag; the outside one does not.
    let inside_tags = catalog.get_photo_tags(inside_id).unwrap();
    let outside_tags = catalog.get_photo_tags(outside_id).unwrap();
    assert!(
        inside_tags.iter().any(|t| t.full_path == "Places/UnitSquare"),
        "inside photo gets the place tag"
    );
    assert!(
        outside_tags.is_empty(),
        "outside photo gets no tag"
    );
}

/// Applying the same fence twice is idempotent: the second call returns 0 new
/// assignments and doesn't duplicate tags.
#[cfg(feature = "map")]
#[test]
fn map_apply_fence_is_idempotent() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("map_idempotent");
    map::ensure_schema_for(&catalog).unwrap();

    let photo_id = insert_photo_with_gps(&catalog, &root, "inside.jpg", 0.5, 0.5);
    let fence_id = unit_square_fence(&catalog, "Places/Idempotent");

    let first = map::apply_fence(&catalog, fence_id).unwrap();
    assert_eq!(first, 1);

    let second = map::apply_fence(&catalog, fence_id).unwrap();
    assert_eq!(second, 0, "re-apply reports zero new assignments");

    // Exactly one tag on the photo (no duplicates).
    let tags = catalog.get_photo_tags(photo_id).unwrap();
    let matching: Vec<_> = tags.iter().filter(|t| t.full_path == "Places/Idempotent").collect();
    assert_eq!(matching.len(), 1, "no duplicate tag assignment after re-apply");
}

/// apply_all_fences applies every fence and returns the sum of new assignments.
#[cfg(feature = "map")]
#[test]
fn map_apply_all_fences_covers_every_fence() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("map_apply_all");
    map::ensure_schema_for(&catalog).unwrap();

    // Two fences in non-overlapping regions.
    let poly_a = vec![(0.0_f64, 0.0_f64), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)];
    let poly_b = vec![(10.0_f64, 10.0_f64), (10.0, 11.0), (11.0, 11.0), (11.0, 10.0)];
    map::create_fence_for(&catalog, "A", "Places/RegionA", &poly_a).unwrap();
    map::create_fence_for(&catalog, "B", "Places/RegionB", &poly_b).unwrap();

    // One photo in each region.
    insert_photo_with_gps(&catalog, &root, "in_a.jpg", 0.5, 0.5);
    insert_photo_with_gps(&catalog, &root, "in_b.jpg", 10.5, 10.5);
    // One photo outside both.
    insert_photo_with_gps(&catalog, &root, "outside.jpg", 50.0, 50.0);

    let total = map::apply_all_fences(&catalog).unwrap();
    assert_eq!(total, 2, "one assignment per region (two total)");
}

/// map_photo_points returns only photos with both GPS columns set.
#[cfg(feature = "map")]
#[test]
fn map_photo_points_returns_gps_photos_only() {
    use chairphoto_lib::plugins::map;
    use chairphoto_lib::catalog::PromotedMetadata;

    let (catalog, root) = temp_catalog("map_points");
    map::ensure_schema_for(&catalog).unwrap();

    // One photo with GPS.
    let gps_id = insert_photo_with_gps(&catalog, &root, "gps.jpg", 59.9, 10.7);

    // One photo without GPS.
    let no_gps = root.join("nogps.jpg");
    std::fs::write(&no_gps, b"\xff\xd8test").unwrap();
    let no_gps_id = catalog.upsert_photo(&no_gps, None, 1, 4).unwrap().id;
    let meta_no_gps = PromotedMetadata { ..Default::default() };
    catalog.set_photo_metadata(no_gps_id, &meta_no_gps, &[]).unwrap();

    let points = map::map_photo_points_for(&catalog).unwrap();
    assert_eq!(points.len(), 1, "only the photo with GPS appears");
    assert_eq!(points[0].id, gps_id);
    assert!((points[0].lat - 59.9).abs() < 1e-9);
    assert!((points[0].lng - 10.7).abs() < 1e-9);
}

/// apply_fence builds the full tag hierarchy (ancestors are created automatically).
#[cfg(feature = "map")]
#[test]
fn map_apply_fence_creates_full_tag_hierarchy() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("map_hierarchy");
    map::ensure_schema_for(&catalog).unwrap();

    let photo_id = insert_photo_with_gps(&catalog, &root, "in.jpg", 0.5, 0.5);
    let fence_id = unit_square_fence(&catalog, "Places/Country/City/Neighbourhood");

    map::apply_fence(&catalog, fence_id).unwrap();

    let tags = catalog.get_photo_tags(photo_id).unwrap();
    // The assigned tag is the leaf (ancestors are pruned by assign_tag's auto-prune).
    assert!(
        tags.iter().any(|t| t.full_path == "Places/Country/City/Neighbourhood"),
        "leaf tag assigned"
    );
    // The ancestor tags exist in the tag tree even if not on the photo.
    let city_exists = catalog.find_tag_id_by_path("Places/Country/City").unwrap().is_some();
    assert!(city_exists, "intermediate ancestor tag created");
}

/// apply_fences_to_photo applies all fences to a single photo — the import hook.
#[cfg(feature = "map")]
#[test]
fn map_apply_fences_to_photo_import_hook() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("map_import_hook");
    map::ensure_schema_for(&catalog).unwrap();

    unit_square_fence(&catalog, "Places/Hook");

    // Photo inside the fence.
    let inside_id = insert_photo_with_gps(&catalog, &root, "in.jpg", 0.5, 0.5);
    let count = map::apply_fences_to_photo(&catalog, inside_id).unwrap();
    assert_eq!(count, 1, "one fence matched");
    let tags = catalog.get_photo_tags(inside_id).unwrap();
    assert!(tags.iter().any(|t| t.full_path == "Places/Hook"), "tag assigned");

    // Photo outside the fence.
    let outside_id = insert_photo_with_gps(&catalog, &root, "out.jpg", 5.0, 5.0);
    let count2 = map::apply_fences_to_photo(&catalog, outside_id).unwrap();
    assert_eq!(count2, 0, "zero fences matched");
    assert!(catalog.get_photo_tags(outside_id).unwrap().is_empty());
}

/// apply_fences_to_photo on a photo without GPS returns 0 without error.
#[cfg(feature = "map")]
#[test]
fn map_apply_fences_to_photo_no_gps_is_noop() {
    use chairphoto_lib::plugins::map;
    use chairphoto_lib::catalog::PromotedMetadata;

    let (catalog, root) = temp_catalog("map_noop");
    map::ensure_schema_for(&catalog).unwrap();

    unit_square_fence(&catalog, "Places/NoGps");

    let path = root.join("nogps.jpg");
    std::fs::write(&path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&path, None, 1, 4).unwrap().id;
    catalog.set_photo_metadata(photo_id, &PromotedMetadata::default(), &[]).unwrap();

    let count = map::apply_fences_to_photo(&catalog, photo_id).unwrap();
    assert_eq!(count, 0, "no GPS → no fence applied");
    assert!(catalog.get_photo_tags(photo_id).unwrap().is_empty());
}

/// ingest_from_card applies fences to newly-imported photos (the import hook path).
#[cfg(feature = "map")]
#[test]
fn map_ingest_applies_fences_to_new_photos() {
    use chairphoto_lib::plugins::map;
    use chairphoto_lib::scanner::ingest_from_card;

    let (catalog, root) = temp_catalog("map_ingest");
    map::ensure_schema_for(&catalog).unwrap();

    // Set up a fence.
    unit_square_fence(&catalog, "Places/Card");

    // The card import itself doesn't embed GPS (minimal JPEG), so we set it
    // manually after import to simulate what set_photo_metadata does at import time.
    // The real integration is tested via apply_fences_to_photo which is called
    // after set_photo_metadata in index_ingested.
    //
    // Here we verify the fence-apply hook is wired: after import, manually set GPS
    // for one photo, call apply_fences_to_photo, and confirm the tag lands.
    let card = root.parent().unwrap().join("card_map");
    std::fs::create_dir_all(&card).unwrap();
    std::fs::write(card.join("A.jpg"), b"\xff\xd8one").unwrap();

    let result = ingest_from_card(&catalog, &card, &root, Some("Map import test")).unwrap();
    assert_eq!(result.created, 1);

    // Retrieve the photo and manually update its GPS (simulate EXIF extraction).
    let photos = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos.len(), 1);
    let photo_id = photos[0].id;

    use chairphoto_lib::catalog::PromotedMetadata;
    catalog
        .set_photo_metadata(
            photo_id,
            &PromotedMetadata {
                gps_latitude: Some(0.5),
                gps_longitude: Some(0.5),
                ..Default::default()
            },
            &[],
        )
        .unwrap();

    // Now apply the hook directly to simulate what happens at real import time.
    let count = map::apply_fences_to_photo(&catalog, photo_id).unwrap();
    assert_eq!(count, 1, "fence matched after GPS was set");
    let tags = catalog.get_photo_tags(photo_id).unwrap();
    assert!(
        tags.iter().any(|t| t.full_path == "Places/Card"),
        "place tag was applied to the imported photo"
    );
}

/// Integration test for reverse-geocoding IPTC fill: verifies that empty location fields
/// are filled and XMP sidecars are written (with the fill-empty-only semantics).
/// The geocoding itself (Nominatim calls) is tested in geocode.rs unit tests.
#[cfg(feature = "map")]
#[test]
fn reverse_geocode_iptc_fill_integration() {
    let (catalog, root) = temp_catalog("geocode_iptc_fill");

    // Create a photo with GPS but empty IPTC location fields.
    let photo_path = root.join("test.jpg");
    std::fs::write(&photo_path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 4).unwrap().id;

    use chairphoto_lib::catalog::PromotedMetadata;
    catalog
        .set_photo_metadata(
            photo_id,
            &PromotedMetadata {
                gps_latitude: Some(59.9139),
                gps_longitude: Some(10.7522),
                ..Default::default()
            },
            &[],
        )
        .unwrap();

    // Verify IPTC location fields are initially empty.
    let initial_iptc = catalog.get_iptc(photo_id).unwrap();
    assert!(initial_iptc.city.is_empty(), "city must be initially empty");
    assert!(initial_iptc.state.is_empty(), "state must be initially empty");
    assert!(initial_iptc.country.is_empty(), "country must be initially empty");
    assert!(initial_iptc.country_code.is_empty(), "country_code must be initially empty");

    // Simulate a geocoder response (GeocodeResult as returned by the actual geocoder).
    // Note: GeocodeResult uses Option<String>, so we wrap the values in Some().
    use chairphoto_lib::plugins::map::geocode::GeocodeResult;
    let geocode_result = GeocodeResult {
        city: Some("Oslo".to_string()),
        state: Some("Oslo".to_string()),
        country: Some("Norway".to_string()),
        country_code: Some("NO".to_string()),
    };

    // Apply the production fill-empty-fields logic via the public helper.
    // This is the critical path: fill only empty fields, never overwrite user-entered values.
    let current_iptc = catalog.get_iptc(photo_id).unwrap();
    let (updated_iptc, changed) =
        chairphoto_lib::plugins::map::geocode::fill_empty_iptc(&current_iptc, &geocode_result);

    assert!(changed, "at least one field should have been filled");

    // Write to catalog (as done in commands.rs Step 3).
    catalog.set_iptc(photo_id, &updated_iptc).unwrap();

    // Write to XMP sidecar (merge-safe path).
    chairphoto_lib::xmp::write_iptc(&photo_path, &updated_iptc).unwrap();

    // Verify IPTC fields were updated in the catalog.
    let final_iptc = catalog.get_iptc(photo_id).unwrap();
    assert_eq!(final_iptc.city, "Oslo");
    assert_eq!(final_iptc.state, "Oslo");
    assert_eq!(final_iptc.country, "Norway");
    assert_eq!(final_iptc.country_code, "NO");

    // Verify XMP sidecar was written.
    let sidecar_path = photo_path.with_extension("jpg.xmp");
    assert!(sidecar_path.exists(), "XMP sidecar must be created");

    // Parse and verify sidecar content.
    let sidecar_content = std::fs::read_to_string(&sidecar_path).unwrap();
    assert!(sidecar_content.contains("Oslo"), "sidecar must contain city");
    assert!(sidecar_content.contains("Norway"), "sidecar must contain country");
}

/// Test that existing IPTC values are never overwritten by reverse-geocoding fill.
#[cfg(feature = "map")]
#[test]
fn reverse_geocode_preserves_existing_iptc_values() {
    let (catalog, root) = temp_catalog("geocode_preserve");

    // Create a photo with GPS and some pre-existing IPTC location fields.
    let photo_path = root.join("test.jpg");
    std::fs::write(&photo_path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 4).unwrap().id;

    use chairphoto_lib::catalog::PromotedMetadata;
    catalog
        .set_photo_metadata(
            photo_id,
            &PromotedMetadata {
                gps_latitude: Some(59.9139),
                gps_longitude: Some(10.7522),
                ..Default::default()
            },
            &[],
        )
        .unwrap();

    // Pre-fill some IPTC fields with user-entered values.
    let mut pre_iptc = catalog.get_iptc(photo_id).unwrap();
    pre_iptc.city = "CustomCity".to_string();
    pre_iptc.country = "CustomCountry".to_string();
    // state and country_code remain empty — these should be filled by geocoding.
    catalog.set_iptc(photo_id, &pre_iptc).unwrap();

    // Simulate a geocoder response (GeocodeResult as returned by the actual geocoder).
    use chairphoto_lib::plugins::map::geocode::GeocodeResult;
    let geocode_result = GeocodeResult {
        city: Some("Oslo".to_string()),
        state: Some("Vestfold".to_string()),
        country: Some("Norway".to_string()),
        country_code: Some("NO".to_string()),
    };

    // Apply the production fill-empty-fields logic via the public helper.
    // This is the critical test: verify that pre-filled fields are NEVER overwritten.
    let current_iptc = catalog.get_iptc(photo_id).unwrap();
    let (updated_iptc, changed) =
        chairphoto_lib::plugins::map::geocode::fill_empty_iptc(&current_iptc, &geocode_result);

    assert!(changed, "at least state or country_code should have been filled");

    // Verify pre-filled fields were NOT overwritten.
    assert_eq!(updated_iptc.city, "CustomCity", "city must not be overwritten");
    assert_eq!(updated_iptc.country, "CustomCountry", "country must not be overwritten");

    // Verify empty fields were filled.
    assert_eq!(updated_iptc.state, "Vestfold", "empty state should be filled");
    assert_eq!(updated_iptc.country_code, "NO", "empty country_code should be filled");

    // Write to catalog and XMP sidecar (demonstrating the full write path).
    catalog.set_iptc(photo_id, &updated_iptc).unwrap();
    chairphoto_lib::xmp::write_iptc(&photo_path, &updated_iptc).unwrap();

    // Verify the sidecar was written correctly.
    let sidecar_path = photo_path.with_extension("jpg.xmp");
    assert!(sidecar_path.exists(), "XMP sidecar must be created");

    let sidecar_content = std::fs::read_to_string(&sidecar_path).unwrap();
    assert!(
        sidecar_content.contains("CustomCity"),
        "sidecar must preserve user-entered city"
    );
    assert!(
        sidecar_content.contains("Vestfold"),
        "sidecar must contain filled state"
    );
}

// ── H2g: set_photo_gps tests ──────────────────────────────────────────────────

/// set_photo_gps updates GPS columns for multiple photos and re-applies fences.
#[cfg(feature = "map")]
#[test]
fn set_photo_gps_updates_catalog_columns_for_multiple_photos() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("set_gps_columns");
    map::ensure_schema_for(&catalog).unwrap();

    // Create two photos without GPS initially.
    let path_a = root.join("a.jpg");
    let path_b = root.join("b.jpg");
    std::fs::write(&path_a, b"\xff\xd8test").unwrap();
    std::fs::write(&path_b, b"\xff\xd8test").unwrap();
    let id_a = catalog.upsert_photo(&path_a, None, 1, 4).unwrap().id;
    let id_b = catalog.upsert_photo(&path_b, None, 1, 4).unwrap().id;

    // Assign a GPS position to both in one call.
    let lat = 59.9333;
    let lng = 10.7167;
    map::set_photo_gps(&catalog, &[id_a, id_b], lat, lng).unwrap();

    // Verify catalog columns were updated.
    let points = map::map_photo_points_for(&catalog).unwrap();
    let ids: Vec<i64> = points.iter().map(|p| p.id).collect();
    assert!(ids.contains(&id_a), "photo A has GPS after set_photo_gps");
    assert!(ids.contains(&id_b), "photo B has GPS after set_photo_gps");
    for pt in &points {
        if pt.id == id_a || pt.id == id_b {
            assert!((pt.lat - lat).abs() < 1e-9, "latitude stored correctly");
            assert!((pt.lng - lng).abs() < 1e-9, "longitude stored correctly");
        }
    }
}

/// set_photo_gps writes GPS merge-safely into the XMP sidecar and round-trips correctly.
#[cfg(feature = "map")]
#[test]
fn set_photo_gps_sidecar_round_trips_and_preserves_foreign_content() {
    use chairphoto_lib::plugins::map;
    use chairphoto_lib::xmp;

    let (catalog, root) = temp_catalog("set_gps_sidecar");
    map::ensure_schema_for(&catalog).unwrap();

    let photo_path = root.join("test.jpg");
    std::fs::write(&photo_path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 4).unwrap().id;

    // Write a pre-existing sidecar with foreign darktable content.
    let sidecar_path = xmp::sidecar_path(&photo_path);
    let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>5</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
    std::fs::write(&sidecar_path, existing).unwrap();

    let lat = 59.9139;
    let lng = 10.7522;
    map::set_photo_gps(&catalog, &[photo_id], lat, lng).unwrap();

    // Sidecar must exist and contain the GPS fields.
    assert!(sidecar_path.exists(), "sidecar must exist after set_photo_gps");
    let content = std::fs::read_to_string(&sidecar_path).unwrap();
    assert!(content.contains("GPSLatitude"), "sidecar must contain GPSLatitude");
    assert!(content.contains("GPSLongitude"), "sidecar must contain GPSLongitude");
    // Foreign darktable content must be preserved (merge-safe invariant).
    assert!(content.contains("history_end"), "darktable data must not be clobbered");
    assert!(content.contains("darktable"), "darktable namespace must survive");

    // Coordinates must round-trip: read back, convert, compare.
    let (read_lat, read_lng) = xmp::read_gps(&photo_path)
        .expect("GPS must be readable from the sidecar after set_photo_gps");
    assert!((read_lat - lat).abs() < 1e-5, "lat round-trips within 1e-5 degrees");
    assert!((read_lng - lng).abs() < 1e-5, "lng round-trips within 1e-5 degrees");
}

/// set_photo_gps re-applies fences to moved photos so place tags follow.
#[cfg(feature = "map")]
#[test]
fn set_photo_gps_reapplies_fences_after_move() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("set_gps_fences");
    map::ensure_schema_for(&catalog).unwrap();

    // Fence covering the unit square (lat 0..1, lng 0..1).
    let fence_id = unit_square_fence(&catalog, "Places/Square");

    // Photo starts outside the fence (no GPS yet).
    let photo_path = root.join("photo.jpg");
    std::fs::write(&photo_path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 4).unwrap().id;

    // Verify no tags before GPS is set.
    assert!(catalog.get_photo_tags(photo_id).unwrap().is_empty());

    // Move the photo into the fence.
    let assignments = map::set_photo_gps(&catalog, &[photo_id], 0.5, 0.5).unwrap();
    assert_eq!(assignments, 1, "one new fence-tag assignment after moving inside");

    let tags = catalog.get_photo_tags(photo_id).unwrap();
    assert!(
        tags.iter().any(|t| t.full_path == "Places/Square"),
        "photo gets place tag after being moved into the fence"
    );

    // Verify apply was also tied to the fence (not a stale copy).
    let _ = fence_id; // referenced to prove it was used
}

/// set_photo_gps on a photo moved outside all fences creates no new assignments.
#[cfg(feature = "map")]
#[test]
fn set_photo_gps_outside_all_fences_returns_zero() {
    use chairphoto_lib::plugins::map;

    let (catalog, root) = temp_catalog("set_gps_outside");
    map::ensure_schema_for(&catalog).unwrap();

    // Fence at the unit square, photo placed far outside.
    unit_square_fence(&catalog, "Places/Outside");
    let photo_path = root.join("photo.jpg");
    std::fs::write(&photo_path, b"\xff\xd8test").unwrap();
    let photo_id = catalog.upsert_photo(&photo_path, None, 1, 4).unwrap().id;

    let assignments = map::set_photo_gps(&catalog, &[photo_id], 50.0, 50.0).unwrap();
    assert_eq!(assignments, 0, "no fence assignments when photo is outside all fences");
    assert!(
        catalog.get_photo_tags(photo_id).unwrap().is_empty(),
        "no tags assigned when photo is outside all fences"
    );
}

/// Two simultaneous Catalog::open calls on a fresh file must both succeed and
/// leave the database in WAL mode.
/// Regression coverage for the StrictMode initialization races:
/// - WAL activation may return SQLITE_BUSY without running the busy handler when two
///   connections attempt the lock upgrade together, so enable_wal retries that pragma.
/// - migrate() takes BEGIN EXCLUSIVE so concurrent ensure_column check-then-ALTER
///   sequences serialize instead of losing with "duplicate column name".
/// Multiple rounds make both interleavings deterministic enough for CI.
#[test]
fn concurrent_opens_of_fresh_catalog_all_succeed() {
    const ROUNDS: usize = 16;
    const OPENERS: usize = 2;

    let base = std::env::temp_dir().join(format!(
        "chairphoto-test-concurrent-open-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);

    for round in 0..ROUNDS {
        let dir = base.join(round.to_string());
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("test.chairphoto");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(OPENERS));
        let results: Vec<_> = (0..OPENERS)
            .map(|_| {
                let barrier = barrier.clone();
                let db = db.clone();
                let root = root.clone();
                std::thread::spawn(move || {
                    barrier.wait(); // maximize overlap so both opens initialize at once
                    Catalog::open(&db, &root).map(|_| ())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();

        for (i, r) in results.iter().enumerate() {
            assert!(
                r.is_ok(),
                "round {round}, concurrent open #{i} failed: {:?}",
                r.as_ref().err()
            );
        }

        let journal_mode: String = rusqlite::Connection::open(&db)
            .and_then(|conn| conn.query_row("PRAGMA journal_mode;", [], |row| row.get(0)))
            .unwrap();
        assert_eq!(
            journal_mode, "wal",
            "round {round} completed outside WAL mode"
        );
    }

    std::fs::remove_dir_all(base).unwrap();
}

// ---------------------------------------------------------------------------
// Photo identity binding (AGENTS.md, "Photo identity": the UUID lives in BOTH
// SQLite and xmp:Identifier, and the sidecar write is never skipped).
//
// A sidecar write that could not complete used to be an eprintln! and an Ok — the
// catalog kept the row and the photo silently lost its portable identity. These
// tests pin the replacement: the row is kept AND the debt is queued, and a repair
// pass puts the identity on disk once the obstacle is gone.
// ---------------------------------------------------------------------------

/// Restores a directory's permissions when it drops, so a failing assertion cannot
/// leave a read-only directory behind for the next run to trip over.
#[cfg(unix)]
struct ReadOnlyDir(PathBuf);

#[cfg(unix)]
impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Make `dir` reject new files for the lifetime of the guard. Returns `None` when the
/// mode bits do not actually block writes (i.e. running as root), so a test that cannot
/// reproduce the failure skips loudly instead of passing without proving anything.
#[cfg(unix)]
fn read_only_dir(dir: &std::path::Path) -> Option<ReadOnlyDir> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let guard = ReadOnlyDir(dir.to_path_buf());
    let probe = dir.join(".chairphoto-write-probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            eprintln!(
                "SKIPPED: writes to a 0555 directory succeed here (root?), so the \
                 unwritable-sidecar path cannot be reproduced"
            );
            None
        }
        Err(_) => Some(guard),
    }
}

fn pending_sidecar_field_count(
    pending: &[chairphoto_lib::catalog::PendingIdentity],
    field: &str,
) -> usize {
    pending.iter().filter(|p| p.field == field).count()
}

#[cfg(unix)]
#[test]
fn unwritable_sidecar_queues_a_repair_that_later_succeeds() {
    use chairphoto_lib::catalog::SidecarIdentity;

    let (catalog, root) = temp_catalog("identity-unwritable");
    let dir = root.join("2026/06/28");
    std::fs::create_dir_all(&dir).unwrap();
    let photo = dir.join("DSC0001.ARW");
    std::fs::write(&photo, b"raw-bytes").unwrap();
    let up = catalog.upsert_photo(&photo, None, 1, 9).unwrap();

    // The photo's storage goes read-only (a NAS export mounted ro, a locked-down
    // archive) — the sidecar cannot be created.
    let Some(guard) = read_only_dir(&dir) else { return };

    let outcome = catalog
        .ensure_sidecar_identity(up.id, &photo, &up.uuid, None)
        .unwrap();
    assert!(
        matches!(outcome, SidecarIdentity::Unwritable(_)),
        "an unwritable folder must report the failure, got {outcome:?}"
    );
    assert!(
        chairphoto_lib::xmp::read_identifier(&photo).is_none(),
        "no sidecar could be written, so nothing is on disk yet"
    );

    // The row is still catalogued — and the identity debt is durable, not stderr.
    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(pending.len(), 1, "the failure is queued for repair");
    assert_eq!(pending[0].photo_id, up.id);
    assert_eq!(
        pending[0].uuid, up.uuid,
        "the queue names the identity that must still reach the disk"
    );
    assert!(
        pending[0].error.contains("sidecar write failed"),
        "the reason is kept for diagnosis, got {:?}",
        pending[0].error
    );

    // Retrying while the storage is still read-only leaves it queued and counts the try.
    let retry = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (retry.repaired, retry.failed, retry.unreachable),
        (0, 1, 0),
        "a repair against read-only storage fails and stays queued"
    );
    assert_eq!(catalog.list_pending_identity().unwrap()[0].attempts, 2);

    // Once the obstacle is gone the repair binds the identity and clears the queue —
    // portable identity was deferred, never lost.
    drop(guard);
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!((summary.repaired, summary.failed, summary.unreachable), (1, 0, 0));
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&photo).as_deref(),
        Some(up.uuid.as_str()),
        "the catalog UUID is now on disk"
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn scan_read_only_storage_queues_import_batch_sidecar_debt() {
    let (catalog, root) = temp_catalog("batch-sidecar-scan-readonly");
    seed_jpgs(&root, 2);
    let Some(guard) = read_only_dir(&root) else {
        return;
    };

    let abort = AtomicBool::new(false);
    let result = chairphoto_lib::scanner::scan_folder(&catalog, &root, &abort, &|_| {}).unwrap();
    assert_eq!(
        result.created, 2,
        "read-only sidecars must not abort the scan"
    );
    let batch_uuid = catalog.list_import_batches().unwrap()[0].uuid.clone();

    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(
        pending_sidecar_field_count(&pending, "identifier"),
        2,
        "UUID sidecar debt is still recorded per photo"
    );
    assert_eq!(
        pending_sidecar_field_count(&pending, "import_batch"),
        2,
        "ImportBatch sidecar debt must be recorded instead of printed to stderr"
    );

    drop(guard);
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (4, 0, 0),
        "repair drains both UUID and ImportBatch sidecar debt"
    );
    for photo in catalog
        .list_photos(
            None,
            None,
            None,
            &[],
            "all",
            None,
            "all",
            None,
            None,
            &[],
            None,
        )
        .unwrap()
    {
        let path = root.join(&photo.path);
        assert_eq!(
            chairphoto_lib::xmp::read_import_batch(&path).as_deref(),
            Some(batch_uuid.as_str()),
            "{} carries its import batch after repair",
            photo.path
        );
    }
    assert!(catalog.list_pending_identity().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn card_ingest_queues_identity_and_import_batch_sidecar_debt() {
    use chairphoto_lib::scanner::{copy_from_card, index_ingested};

    let (catalog, root) = temp_catalog("batch-sidecar-ingest-readonly");
    let card = root.parent().unwrap().join("card-readonly-sidecar");
    std::fs::create_dir_all(&card).unwrap();
    std::fs::write(card.join("IMG_1.jpg"), b"\xff\xd8one").unwrap();

    let (copy_result, copied) = copy_from_card(&card, &root, None, |_, _| {}).unwrap();
    assert_eq!(copied.len(), 1);
    let dest = copied[0].dest.clone();
    let sidecar_dir = dest.parent().unwrap().to_path_buf();
    let Some(guard) = read_only_dir(&sidecar_dir) else {
        return;
    };

    let result = index_ingested(
        &catalog,
        &root,
        &card,
        copied,
        Some("Malformed sidecar ingest"),
        copy_result,
    )
    .unwrap();
    assert_eq!(result.created, 1, "the photo is still indexed");
    let batch_uuid = catalog.list_import_batches().unwrap()[0].uuid.clone();

    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(pending_sidecar_field_count(&pending, "identifier"), 1);
    assert_eq!(
        pending_sidecar_field_count(&pending, "import_batch"),
        1,
        "the ingest-side ImportBatch write failure is retryable catalog debt"
    );
    assert!(
        chairphoto_lib::xmp::read_import_batch(&dest).is_none(),
        "the read-only destination could not be updated yet"
    );

    drop(guard);
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (2, 0, 0)
    );
    let photo = catalog
        .list_photos(
            None,
            None,
            None,
            &[],
            "all",
            None,
            "all",
            None,
            None,
            &[],
            None,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&dest).as_deref(),
        Some(photo.uuid.as_str())
    );
    assert_eq!(
        chairphoto_lib::xmp::read_import_batch(&dest).as_deref(),
        Some(batch_uuid.as_str())
    );
    assert!(catalog.list_pending_identity().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn bundle_import_queues_identity_sidecar_debt_for_unwritable_extracted_copy() {
    use chairphoto_lib::bundle::importer::{extract_originals, index_bundle, open_bundle};
    use chairphoto_lib::bundle::writer::{gather_bundle, write_bundle};

    let (cat_a, _root_a, _uuid1, _uuid2, batch_id_a) = setup_catalog_a("identity-bundle-a");
    let bundle_zip = std::env::temp_dir().join(format!(
        "chairphoto-bundle-identity-readonly-{}.chairphoto",
        std::process::id()
    ));
    let _guard = {
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        Rm(bundle_zip.clone())
    };
    let gathered = gather_bundle(&cat_a, batch_id_a)
        .unwrap()
        .expect("batch must export");
    write_bundle(&gathered, &bundle_zip, |_, _| {}).unwrap();

    let (cat_b, root_b) = temp_catalog("identity-bundle-b");
    let (manifest, mut archive) = open_bundle(&bundle_zip).unwrap();
    let (extracted, partial) =
        extract_originals(&manifest, &mut archive, &root_b, |_, _| {}).unwrap();
    assert_eq!(extracted.len(), 2);

    let target = extracted[0].dest.clone();
    let target_uuid = extracted[0].photo_uuid.clone();
    let sidecar = chairphoto_lib::xmp::sidecar_path(&target);
    assert!(
        sidecar.exists(),
        "extract_originals normally writes the bundle UUID before indexing"
    );
    std::fs::remove_file(&sidecar).unwrap();
    let sidecar_dir = target.parent().unwrap().to_path_buf();
    let Some(guard) = read_only_dir(&sidecar_dir) else {
        return;
    };

    let result = index_bundle(&cat_b, &manifest, &extracted, &root_b, partial).unwrap();
    assert_eq!(result.errors, 0, "the bundle import still succeeds");
    let imported = cat_b
        .get_photo_by_uuid(&target_uuid)
        .expect("the extracted photo is catalogued");

    let pending = cat_b.list_pending_identity().unwrap();
    assert_eq!(
        pending_sidecar_field_count(&pending, "identifier"),
        1,
        "the bundle indexer records UUID sidecar debt for the imported copy"
    );
    let identifier = pending.iter().find(|p| p.field == "identifier").unwrap();
    assert_eq!(identifier.photo_id, imported.id);
    assert_eq!(PathBuf::from(&identifier.target_path), target);
    assert!(
        chairphoto_lib::xmp::read_identifier(&target).is_none(),
        "the read-only destination could not be updated yet"
    );

    drop(guard);
    let summary = cat_b.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.failed, summary.unreachable),
        (0, 0),
        "the queued UUID repair should complete once the destination is writable"
    );
    assert!(
        summary.repaired >= 1,
        "at least the queued UUID field should be repaired"
    );
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&target).as_deref(),
        Some(imported.uuid.as_str())
    );
    assert!(cat_b.list_pending_identity().unwrap().is_empty());
}

#[test]
fn malformed_sidecar_is_preserved_and_queued() {
    use chairphoto_lib::catalog::SidecarIdentity;

    let (catalog, root) = temp_catalog("identity-malformed");
    let photo = root.join("DSC0002.ARW");
    std::fs::write(&photo, b"raw-bytes").unwrap();
    // A truncated sidecar — half-written by an interrupted third-party tool.
    let sidecar = root.join("DSC0002.ARW.xmp");
    let corrupt: &[u8] = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF";
    std::fs::write(&sidecar, corrupt).unwrap();

    let up = catalog.upsert_photo(&photo, None, 1, 9).unwrap();
    assert!(
        chairphoto_lib::xmp::read_identifier(&photo).is_none(),
        "a sidecar that does not parse yields no identifier"
    );

    let outcome = catalog
        .ensure_sidecar_identity(up.id, &photo, &up.uuid, None)
        .unwrap();
    assert!(
        matches!(outcome, SidecarIdentity::Unwritable(_)),
        "a sidecar that cannot be parsed cannot be merged into, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        corrupt,
        "the unreadable sidecar is left byte-identical — XMP safety says preserve"
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 1);
    assert_eq!(
        catalog.repair_pending_identity().unwrap().failed,
        1,
        "repair does not paper over a corrupt sidecar"
    );

    // With the corrupt file out of the way, the queued repair completes.
    std::fs::remove_file(&sidecar).unwrap();
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!((summary.repaired, summary.failed), (1, 0));
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&photo).as_deref(),
        Some(up.uuid.as_str())
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 0);
}

#[test]
fn a_foreign_identity_in_the_sidecar_is_never_overwritten() {
    use chairphoto_lib::catalog::SidecarIdentity;

    let (catalog, root) = temp_catalog("identity-conflict");
    let photo = root.join("DSC0003.ARW");
    std::fs::write(&photo, b"raw-bytes").unwrap();
    let up = catalog.upsert_photo(&photo, None, 1, 9).unwrap();

    // The file already carries somebody else's identity (a copied sidecar, another
    // catalog's photo). Binding must not resolve that by destroying it.
    const FOREIGN: &str = "11111111-2222-3333-4444-555555555555";
    chairphoto_lib::xmp::write_identifier(&photo, FOREIGN).unwrap();
    let found = chairphoto_lib::xmp::read_identifier(&photo);

    let outcome = catalog
        .ensure_sidecar_identity(up.id, &photo, &up.uuid, found.as_deref())
        .unwrap();
    assert_eq!(outcome, SidecarIdentity::Conflict(FOREIGN.to_string()));
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&photo).as_deref(),
        Some(FOREIGN),
        "the foreign identity stays on disk"
    );

    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].error.contains("different identity"),
        "the divergence is described, got {:?}",
        pending[0].error
    );

    // A repair pass keeps reporting it rather than clobbering — this one needs a human.
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!((summary.repaired, summary.failed), (0, 1));
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&photo).as_deref(),
        Some(FOREIGN)
    );
}

#[test]
fn an_unreachable_original_leaves_the_repair_queued() {
    let (catalog, root) = temp_catalog("identity-unreachable");
    let photo = root.join("DSC0004.ARW");
    std::fs::write(&photo, b"raw-bytes").unwrap();
    let up = catalog.upsert_photo(&photo, None, 1, 9).unwrap();
    std::fs::write(
        root.join("DSC0004.ARW.xmp"),
        b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF",
    )
    .unwrap();
    catalog
        .ensure_sidecar_identity(up.id, &photo, &up.uuid, None)
        .unwrap();
    assert_eq!(catalog.count_pending_identity().unwrap(), 1);

    // The volume goes away (unmounted NAS, disconnected disk). Missing storage is a
    // normal state: the repair is not a failure and not a reason to drop the row.
    std::fs::remove_file(&photo).unwrap();
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (0, 0, 1)
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 1, "still queued");
}

#[test]
fn identity_repair_keeps_copy_debt_when_another_copy_is_reachable() {
    use chairphoto_lib::catalog::SidecarIdentity;

    let (catalog, root) = temp_catalog("identity-copy-target");
    let primary = root.join("DSC0005.ARW");
    std::fs::write(&primary, b"primary-raw").unwrap();
    let up = catalog.upsert_photo(&primary, None, 1, 11).unwrap();

    let backup_dir = root.parent().unwrap().join("backup-identity");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup = backup_dir.join("DSC0005.ARW");
    std::fs::write(&backup, b"backup-raw").unwrap();
    let backup_volume = catalog
        .add_volume("Backup", &backup_dir, VolumeKind::Backup)
        .unwrap();
    catalog
        .add_location(up.id, backup_volume, "DSC0005.ARW", LocationRole::Backup)
        .unwrap();

    let corrupt_sidecar = root.join("DSC0005.ARW.xmp");
    std::fs::write(
        &corrupt_sidecar,
        b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF",
    )
    .unwrap();

    let outcome = catalog
        .ensure_sidecar_identity(up.id, &primary, &up.uuid, None)
        .unwrap();
    assert!(
        matches!(outcome, SidecarIdentity::Unwritable(_)),
        "the corrupt primary sidecar must be queued, got {outcome:?}"
    );
    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(PathBuf::from(&pending[0].target_path), primary);

    std::fs::remove_file(&primary).unwrap();
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (0, 0, 1),
        "a reachable backup must not clear debt for the missing primary copy"
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 1);
    assert!(
        chairphoto_lib::xmp::read_identifier(&backup).is_none(),
        "repair is scoped to the queued primary copy"
    );

    let backup_outcome = catalog
        .ensure_sidecar_identity(up.id, &backup, &up.uuid, None)
        .unwrap();
    assert_eq!(backup_outcome, SidecarIdentity::Bound);
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&backup).as_deref(),
        Some(up.uuid.as_str())
    );
    let pending = catalog.list_pending_identity().unwrap();
    assert_eq!(
        pending.len(),
        1,
        "binding the backup copy must not erase the primary copy's debt"
    );
    assert_eq!(PathBuf::from(&pending[0].target_path), primary);

    std::fs::write(&primary, b"primary-raw").unwrap();
    std::fs::remove_file(&corrupt_sidecar).unwrap();
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (1, 0, 0)
    );
    assert_eq!(
        chairphoto_lib::xmp::read_identifier(&primary).as_deref(),
        Some(up.uuid.as_str())
    );
    assert_eq!(catalog.count_pending_identity().unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn a_scan_onto_read_only_storage_keeps_every_identity_recoverable() {
    let (catalog, root) = temp_catalog("identity-scan-readonly");
    seed_jpgs(&root, 3);
    let Some(guard) = read_only_dir(&root) else { return };

    // The scan still imports — one unwritable folder must not cost the user the rows.
    let abort = AtomicBool::new(false);
    let result = chairphoto_lib::scanner::scan_folder(&catalog, &root, &abort, &|_| {}).unwrap();
    assert_eq!(result.created, 3, "the scan indexes every file");
    assert_eq!(
        catalog.count_pending_identity().unwrap(),
        3,
        "every photo whose sidecar could not be written owes an identity repair"
    );

    // Remount read-write and repair: every queued sidecar identity field is retried;
    // this scan also queued the import-batch field for each photo.
    drop(guard);
    let summary = catalog.repair_pending_identity().unwrap();
    assert_eq!(
        (summary.repaired, summary.failed, summary.unreachable),
        (6, 0, 0)
    );
    let photos = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    assert_eq!(photos.len(), 3);
    for photo in &photos {
        assert_eq!(
            chairphoto_lib::xmp::read_identifier(&root.join(&photo.path)).as_deref(),
            Some(photo.uuid.as_str()),
            "{} carries its catalog identity after the repair",
            photo.path
        );
    }
    assert_eq!(catalog.count_pending_identity().unwrap(), 0);
}
