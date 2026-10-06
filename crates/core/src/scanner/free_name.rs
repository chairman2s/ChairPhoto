//! Which library name a file arriving by card ingest or bundle import takes (#247).
//!
//! A name is free only when nothing is on disk there ([`same_photo::name_free`]: no file, no
//! sidecar) **and** no catalog row holds it — by its logical path or by any of its locations,
//! whatever the role, whether its file is there or not. Missing storage is a normal state: a
//! photo whose file was deleted outside ChairPhoto keeps its row, its rating and its tags,
//! and a new file placed at its name would be indexed onto that row (the upsert matches by
//! path), giving the old photo's identity and culling to another capture.
//!
//! One exception, so a photo deleted outside the app and imported again from its card goes
//! back to its row rather than to a ` (n)` beside it (L-f of the third #246 review): a name
//! whose file is gone, held by the logical path of exactly one row, is that row's to
//! re-link when the arriving file **is** that row's photo —
//!
//! - a card's file: #246's rule ([`same_photo::same_capture`]) says the same capture, its
//!   stamp against the one stored for the row (both read by `metadata::extract_batch`). No
//!   capture time on either side is no match: the row's file is gone, so there are no
//!   contents to compare.
//! - a bundle's original: the bundle gives it the row's identity.
//!
//! and nothing at the name's sidecar says otherwise: no sidecar, or one whose every
//! `xmp:Identifier` is the row's. A sidecar of another identity — or of none, or one that
//! does not parse — keeps the name taken. Any other arriving file goes on to the next free
//! ` (n)` and gets a row of its own; a different capture is never attached to an old row.
//!
//! The catalog is read once per folder per run ([`CatalogNames`]), on the connection the
//! import indexes through — the catalog the import started against, never one opened since.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::same_photo::{self, CaptureStamp};
use crate::catalog::{Catalog, NameHolder};

/// What is known of a file arriving in the library, to tell whether a name a catalog row
/// holds is that row's photo coming back ([`CatalogNames::destination`]).
#[derive(Debug, Clone)]
pub enum Arriving {
    /// A card's file: its capture stamp, read from the metadata the import extracted
    /// ([`same_photo::stamp_from_metadata`]).
    Capture(CaptureStamp),
    /// A bundle's original: the identity the bundle gives it, canonical
    /// (`catalog::photo_identity_for`); `None` for a blank one, which re-links nothing.
    Identity(Option<String>),
}

/// The names the catalog holds in each library folder an import places files in, read once
/// per folder per run.
pub struct CatalogNames<'c> {
    catalog: &'c Catalog,
    dirs: HashMap<PathBuf, Folder>,
}

/// One folder's holders, and the stamps of the photos held by path there (read on first
/// need: only a name whose file is gone needs them).
struct Folder {
    /// `None` when the catalog could not be read: no name in the folder is free.
    held: Option<HashMap<String, Vec<NameHolder>>>,
    stamps: Option<HashMap<i64, CaptureStamp>>,
}

impl<'c> CatalogNames<'c> {
    /// Names as `catalog` holds them. `catalog` must be the catalog the import indexes into.
    pub fn new(catalog: &'c Catalog) -> Self {
        Self { catalog, dirs: HashMap::new() }
    }

    /// Where a file arriving as `wanted` goes: the name of a row it re-links (the module
    /// doc), else `wanted` or the first ` (n)` beside it that is free on disk and held by no
    /// row. `None` when there is none, or the catalog could not be read for the folder —
    /// the caller then copies nothing.
    pub fn destination(&mut self, wanted: &Path, arriving: &Arriving) -> Option<PathBuf> {
        if let Some(relink) = self.relink_target(wanted, arriving) {
            return Some(relink);
        }
        let dir = wanted.parent()?;
        std::iter::once(wanted.to_path_buf())
            .chain((2..10_000).map(|n| same_photo::numbered(wanted, n)))
            .find(|c| same_photo::name_free(c) && self.held_by(dir, c).is_some_and(|h| h.is_empty()))
    }

    /// The rows holding `path` (empty: none); `None` when the folder could not be read.
    fn held_by(&mut self, dir: &Path, path: &Path) -> Option<&[NameHolder]> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        let held = self.folder(dir).held.as_ref()?;
        Some(held.get(&name).map_or(&[], Vec::as_slice))
    }

    fn folder(&mut self, dir: &Path) -> &mut Folder {
        let catalog = self.catalog;
        self.dirs.entry(dir.to_path_buf()).or_insert_with(|| {
            let held = catalog
                .names_held_in(dir)
                .inspect_err(|e| eprintln!("import: couldn't read the names the catalog holds in {}: {e}", dir.display()))
                .ok();
            Folder { held, stamps: None }
        })
    }

    /// The first name — `wanted`, then its ` (n)` names in order of `n` — that `arriving`
    /// re-links ([`Self::relinks`]).
    fn relink_target(&mut self, wanted: &Path, arriving: &Arriving) -> Option<PathBuf> {
        let dir = wanted.parent()?;
        let base = wanted.file_name()?.to_string_lossy().into_owned();
        let key = (
            wanted.file_stem()?.to_string_lossy().into_owned(),
            wanted.extension().map(|e| e.to_string_lossy().into_owned()),
        );
        let held = self.folder(dir).held.as_ref()?;
        let mut names: Vec<(u32, String)> = held
            .keys()
            .filter_map(|name| {
                if *name == base {
                    Some((1, name.clone()))
                } else {
                    same_photo::numbered_parts(name).filter(|(k, _)| *k == key).map(|(_, n)| (n, name.clone()))
                }
            })
            .collect();
        names.sort();
        names.into_iter().map(|(_, name)| dir.join(name)).find(|path| self.relinks(dir, path, arriving))
    }

    /// Whether `arriving` goes to `path` to re-link the row holding it: its file is gone, one
    /// row holds the name, by its logical path; nothing at the sidecar's name says another
    /// identity; and `arriving` is that row's photo.
    fn relinks(&mut self, dir: &Path, path: &Path, arriving: &Arriving) -> bool {
        if !same_photo::absent(path) {
            return false; // a file there is #246's to decide, as a candidate
        }
        let Some(holders) = self.held_by(dir, path) else { return false };
        let [holder] = holders else { return false };
        if !holder.by_path {
            return false;
        }
        let (photo_id, uuid) = (holder.photo_id, holder.uuid.clone());
        if !sidecar_is_the_rows(path, &uuid) {
            return false;
        }
        match arriving {
            Arriving::Identity(identity) => identity.as_deref() == Some(uuid.as_str()),
            Arriving::Capture(stamp) => {
                let stored = self.stamp_of(dir, photo_id);
                same_photo::same_capture(stamp, &stored) == Some(true)
            }
        }
    }

    /// The capture stamp stored for `photo_id`, a photo held by path in `dir`: the folder's
    /// stamps are read in one query, the first time one is needed. None stored (or a catalog
    /// that cannot be read) is no capture time, which matches nothing.
    fn stamp_of(&mut self, dir: &Path, photo_id: i64) -> CaptureStamp {
        let catalog = self.catalog;
        let folder = self.folder(dir);
        let stamps = folder.stamps.get_or_insert_with(|| {
            let rows = catalog.capture_metadata_in(dir).unwrap_or_else(|e| {
                eprintln!("import: couldn't read the capture metadata stored for {}: {e}", dir.display());
                Vec::new()
            });
            let mut by_photo: HashMap<i64, Vec<(String, String, String)>> = HashMap::new();
            for (id, key, group, value) in rows {
                by_photo.entry(id).or_default().push((key, group, value));
            }
            by_photo
                .into_iter()
                .map(|(id, entries)| {
                    let stamp = same_photo::stamp_from_metadata(
                        entries.iter().map(|(k, g, v)| (k.as_str(), g.as_str(), v.as_str())),
                    );
                    (id, stamp)
                })
                .collect()
        });
        stamps.get(&photo_id).cloned().unwrap_or_default()
    }
}

/// Whether the sidecar at `path`'s sidecar name is no other photo's: there is none, or every
/// `xmp:Identifier` it carries is `uuid` (canonically). One with no identifier, or that does
/// not parse, is not known to be the row's.
fn sidecar_is_the_rows(path: &Path, uuid: &str) -> bool {
    if same_photo::absent(&crate::xmp::sidecar_path(path)) {
        return true;
    }
    let ids = crate::xmp::read_identifiers(path);
    !ids.is_empty() && ids.iter().all(|v| crate::catalog::photo_identity_for(v).as_deref() == Some(uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";

    fn rig(tag: &str) -> (crate::test_support::TestTmpDir, Catalog, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("free-name-{tag}"));
        let root = dir.join("lib");
        let day = root.join("2026/06/28");
        std::fs::create_dir_all(&day).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        (dir, catalog, day)
    }

    /// A row at `path` with identity `uuid` and the stamp's capture metadata, its file then
    /// deleted outside the app (the row stays).
    fn row_whose_file_is_gone(catalog: &Catalog, path: &Path, uuid: &str, time: &str) -> i64 {
        std::fs::write(path, b"the old capture").unwrap();
        let id = catalog.upsert_photo_with_identity(path, None, 1, 15, Some(uuid)).unwrap().id;
        let entries = [("DateTimeOriginal", "EXIF", time), ("SubSecTimeOriginal", "EXIF", "12"), ("SerialNumber", "EXIF", "S1")]
            .map(|(key, group, value)| crate::catalog::MetadataEntry {
                key: key.into(),
                group_name: group.into(),
                value: value.into(),
            });
        catalog.set_photo_metadata(id, &Default::default(), &entries).unwrap();
        std::fs::remove_file(path).unwrap();
        id
    }

    fn capture(time: &str) -> Arriving {
        Arriving::Capture(same_photo::stamp_from_metadata([
            ("DateTimeOriginal", "EXIF", time),
            ("SubSecTimeOriginal", "EXIF", "12"),
            ("SerialNumber", "EXIF", "S1"),
        ]))
    }

    const T: &str = "2026:06:28 12:00:00";
    const T2: &str = "2026:06:28 12:00:01";

    // --- held names (#247) -------------------------------------------------------------

    /// #247: a name a row still holds, its file deleted outside the app, is not free for
    /// another capture: it goes to the next name no row holds.
    #[test]
    fn a_name_a_row_holds_is_not_free_for_another_capture() {
        let (_dir, catalog, day) = rig("held");
        std::fs::write(day.join("IMG.jpg"), b"present").unwrap();
        row_whose_file_is_gone(&catalog, &day.join("IMG (2).jpg"), ROW, T);
        let mut names = CatalogNames::new(&catalog);
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T2)), Some(day.join("IMG (3).jpg")));
        assert_eq!(names.destination(&day.join("IMG.jpg"), &Arriving::Identity(Some(OTHER.into()))), Some(day.join("IMG (3).jpg")));
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T)), Some(day.join("IMG (2).jpg")), "its own capture re-links it");
    }

    /// A name held only by a location — another role, another volume — is held too.
    #[test]
    fn a_name_held_by_a_location_of_any_role_is_not_free() {
        let (_dir, catalog, day) = rig("location");
        let other = day.join("elsewhere.jpg");
        std::fs::write(&other, b"x").unwrap();
        let id = catalog.upsert_photo_with_identity(&other, None, 1, 1, Some(ROW)).unwrap().id;
        let volume = catalog.ensure_default_volume().unwrap();
        catalog.add_location(id, volume, "2026/06/28/IMG.jpg", crate::catalog::LocationRole::Backup).unwrap();
        let mut names = CatalogNames::new(&catalog);
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T)), Some(day.join("IMG (2).jpg")));
        assert_eq!(
            names.destination(&day.join("IMG.jpg"), &Arriving::Identity(Some(ROW.into()))),
            Some(day.join("IMG (2).jpg")),
            "a location-only holder is never re-linked"
        );
    }

    /// The folder is read once per run, not once per name asked.
    #[test]
    fn each_folder_is_read_once_per_run() {
        let (_dir, catalog, day) = rig("once");
        row_whose_file_is_gone(&catalog, &day.join("IMG.jpg"), ROW, T);
        let mut names = CatalogNames::new(&catalog);
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T2)), Some(day.join("IMG (2).jpg")));
        // A row added after the folder was read is not seen by this run…
        std::fs::write(day.join("IMG (2).jpg"), b"x").unwrap();
        catalog.upsert_photo_with_identity(&day.join("IMG (2).jpg"), None, 1, 1, Some(OTHER)).unwrap();
        std::fs::remove_file(day.join("IMG (2).jpg")).unwrap();
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T2)), Some(day.join("IMG (2).jpg")));
        // …and a new run sees it.
        assert_eq!(CatalogNames::new(&catalog).destination(&day.join("IMG.jpg"), &capture(T2)), Some(day.join("IMG (3).jpg")));
    }

    // --- re-linking (L-f of the third #246 review) -------------------------------------

    /// The row's own sidecar left behind does not stop its capture coming back to it; a
    /// sidecar of another identity, or of none, keeps the name taken.
    #[test]
    fn only_a_sidecar_of_the_rows_identity_lets_it_be_re_linked() {
        let (_dir, catalog, day) = rig("sidecar");
        let name = day.join("IMG.jpg");
        row_whose_file_is_gone(&catalog, &name, ROW, T);
        same_photo::test_files::orphan_sidecar(&name, ROW);
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &capture(T)), Some(name.clone()));
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &Arriving::Identity(Some(ROW.into()))), Some(name.clone()));

        std::fs::remove_file(crate::xmp::sidecar_path(&name)).unwrap();
        same_photo::test_files::orphan_sidecar(&name, OTHER);
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &capture(T)), Some(day.join("IMG (2).jpg")));
        assert_eq!(
            CatalogNames::new(&catalog).destination(&name, &Arriving::Identity(Some(ROW.into()))),
            Some(day.join("IMG (2).jpg"))
        );

        std::fs::write(crate::xmp::sidecar_path(&name), b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>").unwrap();
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &capture(T)), Some(day.join("IMG (2).jpg")));
    }

    /// A row that matches is re-linked at its ` (n)` name even when the plain name is free:
    /// the photo goes back to the name its row holds.
    #[test]
    fn a_matching_row_at_a_numbered_name_is_preferred_over_a_free_name() {
        let (_dir, catalog, day) = rig("numbered");
        row_whose_file_is_gone(&catalog, &day.join("IMG (3).jpg"), ROW, T);
        let mut names = CatalogNames::new(&catalog);
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T)), Some(day.join("IMG (3).jpg")));
        assert_eq!(names.destination(&day.join("IMG.jpg"), &capture(T2)), Some(day.join("IMG.jpg")));
    }

    /// No capture time stored (or arriving) is no match: the row's file is gone, so no
    /// contents can decide.
    #[test]
    fn without_a_capture_time_nothing_is_re_linked() {
        let (_dir, catalog, day) = rig("no-time");
        let name = day.join("IMG.png");
        std::fs::write(&name, b"png").unwrap();
        catalog.upsert_photo_with_identity(&name, None, 1, 3, Some(ROW)).unwrap();
        std::fs::remove_file(&name).unwrap();
        let none = Arriving::Capture(CaptureStamp::default());
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &none), Some(day.join("IMG (2).png")));
        assert_eq!(CatalogNames::new(&catalog).destination(&name, &capture(T)), Some(day.join("IMG (2).png")));
    }

    /// The stamps compared come from the same groups on both sides: EXIF before any other
    /// group, and `CreateDate` only from QuickTime.
    #[test]
    fn the_stored_stamp_reads_the_groups_the_exiftool_stamp_reads() {
        let s = same_photo::stamp_from_metadata([
            ("DateTimeOriginal", "XMP", "2000:01:01 00:00:00"),
            ("DateTimeOriginal", "EXIF", T),
            ("CreateDate", "EXIF", "2001:01:01 00:00:00"),
            ("SerialNumber", "MakerNotes", " S9 "),
        ]);
        assert_eq!(s.time.as_deref(), Some(T));
        assert_eq!(s.created, None);
        assert_eq!(s.serials.get("SerialNumber").map(String::as_str), Some("S9"));
        let video = same_photo::stamp_from_metadata([("CreateDate", "QuickTime", T)]);
        assert_eq!(video.created.as_deref(), Some(T));
    }
}
