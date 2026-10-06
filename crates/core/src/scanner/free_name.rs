//! Which library name a file arriving by card ingest or bundle import takes (#247).
//!
//! A name is free only when nothing is on disk there ([`same_photo::name_free`]: no file, no
//! sidecar) **and** no catalog row holds it — by its logical path, or by one of its locations
//! (any role) read under its own volume's base — whether its file is there or not. Missing
//! storage is a normal state: a
//! photo whose file was deleted outside ChairPhoto keeps its row, its rating and its tags,
//! and a new file placed at its name would be indexed onto that row (the upsert matches by
//! path), giving the old photo's identity and culling to another capture.
//!
//! One exception, so a photo deleted outside the app and imported again from its card goes
//! back to its row rather than to a ` (n)` beside it (L-f of the third #246 review): a name
//! whose file is gone, held by the logical path of exactly one row, is that row's to
//! re-link when the arriving file **is** that row's photo —
//!
//! - a card's file: its stamp against the one stored for the row (both read by
//!   `metadata::extract_batch`) proves the same capture without contents to compare
//!   ([`same_photo::same_capture_without_contents`]): #246's rule says the same capture,
//!   and a sub-second or a serial is on both sides (and equal). The same second with a
//!   serial missing on either side and no sub-second on both is not proof — another body
//!   can have shot the same name in that second — and no capture time is no match.
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
    /// The same holders by [`folded`] name: what decides whether a name is held.
    folded: HashMap<String, Vec<NameHolder>>,
    stamps: Option<HashMap<i64, CaptureStamp>>,
}

/// A file name as a case-insensitive filesystem compares it (#231 F4). A library on exFAT,
/// FAT, a casefolded ext4 folder or APFS holds `IMG.JPG` and `img.jpg` as one file; on a
/// case-sensitive filesystem they are two. Rather than probe each folder's filesystem, a
/// row holds every case variant of its name everywhere: on a case-sensitive library that
/// costs at most a ` (n)` name for a file that differs from a row's name only in case,
/// while on a case-insensitive one it keeps a new file from landing at a gone row's name
/// and being indexed onto that row. Simple lowercase only: Unicode normalisation (NFC vs
/// NFD, which APFS and casefold also ignore) is not applied — camera file names are ASCII.
pub(crate) fn folded(name: &str) -> String {
    name.to_lowercase()
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
        // A ` (n)` name whose sidecar could not be written ends the search: every later one is
        // as long or longer (relB2 LOW-3; the caller reports it, `same_photo::numbered_fits`).
        std::iter::once(wanted.to_path_buf())
            .chain((2..10_000).map(|n| same_photo::numbered(wanted, n)))
            .take_while(|c| same_photo::sidecar_name_fits(c))
            .find(|c| same_photo::name_free(c) && self.held_by(dir, c).is_some_and(|h| h.is_empty()))
    }

    /// The row an arriving file would re-link at `wanted` (or a ` (n)` beside it) when that
    /// row's photo was **offloaded** (#231 F5): it has no location on a local volume left —
    /// an offload drops those rows once the backup is verified — and a backup location with a
    /// verified hash. Such a photo's local file is gone on purpose, so a card or bundle
    /// bringing it again has nothing to add, and copying it back would undo the offload. The
    /// record decides, not a look at the backup volume: an unmounted NAS is normal.
    ///
    /// A photo that still has a local location row lost its file some other way (deleted
    /// outside the app, a failed disk): the arriving file may be its last copy, so it is
    /// re-linked as before. When the catalog cannot say, the answer is `None` — copy.
    pub fn kept_elsewhere(&mut self, wanted: &Path, arriving: &Arriving) -> Option<i64> {
        let path = self.relink_target(wanted, arriving)?;
        let photo_id = self.held_by(path.parent()?, &path)?.first()?.photo_id;
        let offloaded: bool = self
            .catalog
            .conn()
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                                    WHERE l.photo_id = ?1 AND v.kind = 'local')
                        AND EXISTS(SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                                    WHERE l.photo_id = ?1 AND v.kind = 'backup' AND l.verified_hash IS NOT NULL)",
                [photo_id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        offloaded.then_some(photo_id)
    }

    /// The rows holding `path` or any case variant of its name ([`folded`]; empty: none);
    /// `None` when the folder could not be read.
    fn held_by(&mut self, dir: &Path, path: &Path) -> Option<&[NameHolder]> {
        let name = folded(&path.file_name()?.to_string_lossy());
        let folder = self.folder(dir);
        folder.held.as_ref()?;
        Some(folder.folded.get(&name).map_or(&[], Vec::as_slice))
    }

    fn folder(&mut self, dir: &Path) -> &mut Folder {
        let catalog = self.catalog;
        self.dirs.entry(dir.to_path_buf()).or_insert_with(|| {
            let held = catalog
                .names_held_in(dir)
                .inspect_err(|e| eprintln!("import: couldn't read the names the catalog holds in {}: {e}", dir.display()))
                .ok();
            let mut by_folded: HashMap<String, Vec<NameHolder>> = HashMap::new();
            for (name, holders) in held.iter().flatten() {
                let entry = by_folded.entry(folded(name)).or_default();
                for h in holders {
                    if !entry.iter().any(|e| e.photo_id == h.photo_id) {
                        entry.push(h.clone());
                    }
                }
            }
            Folder { held, folded: by_folded, stamps: None }
        })
    }

    /// The first name — `wanted`, then its ` (n)` names in order of `n` — that `arriving`
    /// re-links ([`Self::relinks`]).
    pub(crate) fn relink_target(&mut self, wanted: &Path, arriving: &Arriving) -> Option<PathBuf> {
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
        let (photo_id, uuid) = (holder.photo_id, holder.uuid.clone());
        // Held by its logical path under this very spelling: the upsert matches the path
        // byte for byte, so a row whose path is another case variant is not re-linked here.
        let exact = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let by_path = exact.as_ref().and_then(|n| self.folder(dir).held.as_ref()?.get(n)).is_some_and(|hs| {
            hs.iter().any(|h| h.photo_id == photo_id && h.by_path)
        });
        if !by_path {
            return false;
        }
        if !sidecar_is_the_rows(path, &uuid) {
            return false;
        }
        match arriving {
            Arriving::Identity(identity) => identity.as_deref() == Some(uuid.as_str()),
            Arriving::Capture(stamp) => {
                let stored = self.stamp_of(dir, photo_id);
                same_photo::same_capture_without_contents(stamp, &stored)
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

    /// #231 F4: a name is held in every case variant, so on a case-insensitive library
    /// (exFAT, casefold ext4, APFS) a new `IMG.jpg` never lands on a gone row's `IMG.JPG`
    /// — one file there — and is indexed onto that row. A re-link needs the row's own
    /// spelling (the upsert matches the path byte for byte).
    #[test]
    fn a_name_is_held_in_every_case_variant() {
        let (_dir, catalog, day) = rig("case");
        row_whose_file_is_gone(&catalog, &day.join("IMG.JPG"), ROW, T);
        let mut names = CatalogNames::new(&catalog);
        assert_eq!(names.destination(&day.join("img.jpg"), &capture(T2)), Some(day.join("img (2).jpg")));
        assert_eq!(names.destination(&day.join("IMG.jpg"), &Arriving::Identity(Some(OTHER.into()))), Some(day.join("IMG (2).jpg")));
        assert_eq!(
            names.destination(&day.join("IMG.jpg"), &capture(T)),
            Some(day.join("IMG (2).jpg")),
            "its capture under another spelling is not re-linked"
        );
        assert_eq!(names.destination(&day.join("IMG.JPG"), &capture(T)), Some(day.join("IMG.JPG")), "its own spelling is");
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

    /// F1 of the #247 review: with no contents left to compare, the stamps must tell this
    /// capture from another body's in the same second. A serial missing on either side
    /// (the catalog's `-fast2` extraction drops MakerNotes serials) with no sub-second on
    /// both re-links nothing; a sub-second on both, or a serial on both, equal, re-links;
    /// differing serials do not.
    #[test]
    fn a_re_link_needs_a_sub_second_or_a_serial_on_both_sides() {
        let entries = |pairs: &[(&'static str, &'static str)]| -> Vec<(&'static str, &'static str, &'static str)> {
            std::iter::once(("DateTimeOriginal", "EXIF", T)).chain(pairs.iter().map(|(k, v)| (*k, "EXIF", *v))).collect()
        };
        let cases: [(&[(&str, &str)], &[(&str, &str)], bool); 6] = [
            (&[("SerialNumber", "S1")], &[], false),
            (&[], &[("SerialNumber", "S2")], false),
            (&[], &[], false),
            (&[("SubSecTimeOriginal", "12")], &[("SubSecTimeOriginal", "12")], true),
            (&[("SerialNumber", "S1")], &[("SerialNumber", "S1")], true),
            (&[("SerialNumber", "S1")], &[("SerialNumber", "S2")], false),
        ];
        for (i, (stored, arriving, relinks)) in cases.into_iter().enumerate() {
            let (_dir, catalog, day) = rig(&format!("proof-{i}"));
            let name = day.join("IMG_0001.JPG");
            std::fs::write(&name, b"the old capture").unwrap();
            let id = catalog.upsert_photo_with_identity(&name, None, 1, 15, Some(ROW)).unwrap().id;
            let stored: Vec<crate::catalog::MetadataEntry> = entries(stored)
                .into_iter()
                .map(|(key, group, value)| crate::catalog::MetadataEntry {
                    key: key.into(),
                    group_name: group.into(),
                    value: value.into(),
                })
                .collect();
            catalog.set_photo_metadata(id, &Default::default(), &stored).unwrap();
            std::fs::remove_file(&name).unwrap();
            let arriving = Arriving::Capture(same_photo::stamp_from_metadata(entries(arriving)));
            let expected = if relinks { name.clone() } else { day.join("IMG_0001 (2).JPG") };
            assert_eq!(CatalogNames::new(&catalog).destination(&name, &arriving), Some(expected), "case {i}");
        }
    }

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
