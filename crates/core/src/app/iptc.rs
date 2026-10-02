//! Saving a photo's authored IPTC fields: the catalog, then the photo's XMP sidecar.
//!
//! Shared by the Tauri `set_iptc` command and the GPUI inspector (gpui #108).

use super::{with_catalog, AppState};
use crate::catalog::IptcFields;

/// Store `fields` in the catalog, then write the change to the photo's XMP sidecar
/// (merge-safe: `xmp::write_iptc` touches only the managed IPTC fields whose catalog value
/// this save changed, so a sidecar value ChairPhoto never imported survives a save that
/// left that field alone — issue #144). The sidecar sits next to the original the location
/// resolver finds. With no reachable copy the save fails closed: the error says why, and
/// neither the catalog nor the sidecar changes, so a retry once the original is back writes
/// the whole change (a stored change with no sidecar write would make a retry a no-op).
///
/// Blocking: the catalog lock is held only for the path lookup, the read of the previous
/// values and the store — one hold, so the change written is the change stored — and the
/// sidecar's read-modify-write runs after it is released. Call it off the UI thread.
///
/// Overlapping saves of one photo store and write in one order (issue #149): a save first
/// takes the sidecar's write turn ([`crate::xmp::lock::WriteOrder`], reserved under the
/// catalog lock, waited for with none held) and stores and writes while it holds it, so a
/// later save never has its change overwritten by an earlier one that reached the disk last.
/// Storing only once the turn is held also means a volume that went away while the save
/// waited fails the store, leaving the catalog unchanged, rather than the sidecar write.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<(), String> {
    let turn = with_catalog(state, |c| reserve(c, photo_id))?.wait();
    let (original, before) = with_catalog(state, |c| store(c, photo_id, fields))?;
    write(&original, &before, fields, turn)
}

/// [`save_iptc`] of a photo read from the catalog `expected` names: the store and the path
/// lookup fail closed with `CATALOG_CHANGED` once another catalog is open, so neither the
/// other catalog's row with the same id nor its photo's sidecar is written.
pub fn save_iptc_as(
    state: &AppState,
    expected: super::CatalogIdentity,
    photo_id: i64,
    fields: &IptcFields,
) -> Result<(), String> {
    let turn = super::with_catalog_as(state, expected, |c| reserve(c, photo_id))?.wait();
    let (original, before) = super::with_catalog_as(state, expected, |c| store(c, photo_id, fields))?;
    write(&original, &before, fields, turn)
}

/// Reserve the place in line of the photo's sidecar write — under the catalog lock, which
/// never waits on a turn. An unreachable original fails here, before anything is stored.
fn reserve(c: &crate::catalog::Catalog, photo_id: i64) -> crate::catalog::Result<crate::xmp::lock::WriteOrder> {
    Ok(crate::xmp::lock::WriteOrder::reserve(&c.require_photo_path(photo_id)?))
}

/// Store `fields`, returning the original's path and the values they replaced. Called with
/// the write turn held. The path is resolved before the row is written (as the geocoder's
/// `fill_in` does), so an original that is unreachable now — gone while this save waited
/// for its turn included — leaves the catalog unchanged.
fn store(
    c: &crate::catalog::Catalog,
    photo_id: i64,
    fields: &IptcFields,
) -> crate::catalog::Result<(std::path::PathBuf, IptcFields)> {
    let original = c.require_photo_path(photo_id)?;
    let before = c.get_iptc(photo_id)?;
    c.set_iptc(photo_id, fields)?;
    Ok((original, before))
}

/// The sidecar half of a save, off the catalog lock, with the write turn still held: the
/// next save in line goes once this write is done.
fn write(
    original: &std::path::Path,
    before: &IptcFields,
    after: &IptcFields,
    _turn: crate::xmp::lock::WriteOrder,
) -> Result<(), String> {
    crate::xmp::write_iptc(original, before, after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::test_fixtures::{assert_non_iptc_intact, foreign_iptc, iptc, with, FOREIGN};

    /// A catalog with one photo whose sidecar is `sidecar`, a foreign file ChairPhoto has
    /// never written (its IPTC is empty in the catalog). Returns the sidecar's path.
    fn foreign_photo(tag: &str, sidecar: &str) -> (crate::test_support::TestTmpDir, AppState, i64, std::path::PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("DSC144.ARW");
        std::fs::write(&file, b"raw").unwrap();
        std::fs::write(crate::xmp::sidecar_path(&file), sidecar).unwrap();
        let catalog = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let id = catalog.upsert_photo(&file, None, 0, 1).unwrap().id;
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        let xmp = crate::xmp::sidecar_path(&file);
        (dir, state, id, xmp)
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// Issue #144: a save that sets the title writes the title. The creator, rights and every
    /// other value another tool wrote — empty in the catalog before and after — survive.
    #[test]
    fn a_save_that_sets_the_title_keeps_the_foreign_creator_and_rights() {
        for (layout, sidecar) in FOREIGN {
            let (_dir, state, id, xmp) = foreign_photo("iptc-144-title", sidecar);
            save_iptc(&state, id, &IptcFields { title: "Mine".into(), ..Default::default() }).unwrap();

            let xml = read(&xmp);
            assert_eq!(iptc(&xml), with(foreign_iptc(), "dc:title", &["Mine"]), "{layout}:\n{xml}");
            assert_non_iptc_intact(&xml, layout);
        }
    }

    /// A field ChairPhoto had set and the user then clears is removed from the sidecar; the
    /// foreign values beside it stay.
    #[test]
    fn a_save_that_clears_a_field_chairphoto_set_removes_it() {
        for (layout, sidecar) in FOREIGN {
            let (_dir, state, id, xmp) = foreign_photo("iptc-144-clear", sidecar);
            let set = IptcFields { title: "Mine".into(), headline: "Ours".into(), ..Default::default() };
            save_iptc(&state, id, &set).unwrap();
            save_iptc(&state, id, &IptcFields { headline: "Ours".into(), ..Default::default() }).unwrap();

            let xml = read(&xmp);
            let expected = with(with(foreign_iptc(), "dc:title", &[]), "photoshop:Headline", &["Ours"]);
            assert_eq!(iptc(&xml), expected, "{layout}:\n{xml}");
            assert_non_iptc_intact(&xml, layout);
        }
    }

    /// A save while the original is offline fails closed: an error, the catalog row and the
    /// sidecar unchanged. Once the original is back, the retry — the same fields, as the
    /// still-dirty form sends them — writes the whole change. (Storing first made the retry's
    /// diff empty, so it reported success and never wrote the sidecar: review of 3ce3835, H1.)
    #[test]
    fn an_offline_save_changes_nothing_and_the_retry_writes_the_sidecar() {
        let (dir, state, id, xmp) = foreign_photo("iptc-144-offline", crate::xmp::test_fixtures::LIGHTROOM);
        let file = dir.join("library").join("DSC144.ARW");
        let away = dir.join("DSC144.ARW.away");
        std::fs::rename(&file, &away).unwrap();

        let typed = IptcFields { title: "Mine".into(), creator: "Me".into(), ..Default::default() };
        let err = save_iptc(&state, id, &typed).unwrap_err();
        assert!(err.contains("no reachable copy"), "{err}");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, IptcFields::default(), "an offline save must not change the catalog");
        assert_eq!(read(&xmp), crate::xmp::test_fixtures::LIGHTROOM, "nor the sidecar");

        std::fs::rename(&away, &file).unwrap();
        save_iptc(&state, id, &typed).unwrap();
        let xml = read(&xmp);
        let expected = with(with(foreign_iptc(), "dc:title", &["Mine"]), "dc:creator", &["Me"]);
        assert_eq!(iptc(&xml), expected, "the retry must write the change:\n{xml}");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, typed);
    }

    /// A creator the user sets replaces the foreign one wherever it sits — Description #2 in
    /// exiftool's layout — leaving exactly ChairPhoto's; a value saved earlier and unchanged
    /// now is neither dropped nor duplicated.
    #[test]
    fn a_save_that_sets_the_creator_replaces_the_foreign_creator_in_every_description() {
        for (layout, sidecar) in FOREIGN {
            let (_dir, state, id, xmp) = foreign_photo("iptc-144-creator", sidecar);
            let titled = IptcFields { title: "Mine".into(), ..Default::default() };
            save_iptc(&state, id, &titled).unwrap();
            save_iptc(&state, id, &IptcFields { creator: "Andreas".into(), ..titled }).unwrap();

            let xml = read(&xmp);
            let expected = with(with(foreign_iptc(), "dc:title", &["Mine"]), "dc:creator", &["Andreas"]);
            assert_eq!(iptc(&xml), expected, "{layout}:\n{xml}");
            assert_non_iptc_intact(&xml, layout);
        }
    }

    /// Issue #149, probe P2: two overlapping saves of one photo. The first holds its turn,
    /// between storing t1 and writing it; a second (t2 and a creator) arrives. The second
    /// must wait for the first's write, so the sidecar ends where the catalog does — t2 —
    /// instead of the first save's late write putting t1 back.
    #[test]
    fn overlapping_saves_write_the_sidecar_in_the_order_they_stored() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-149-order", crate::xmp::test_fixtures::LIGHTROOM);
        let state = std::sync::Arc::new(state);
        let first = IptcFields { title: "t1".into(), ..Default::default() };
        let turn = with_catalog(&state, |c| reserve(c, id)).unwrap().wait();
        let (original, before) = with_catalog(&state, |c| store(c, id, &first)).unwrap();

        let second = IptcFields { title: "t2".into(), creator: "c".into(), ..Default::default() };
        let later = {
            let (state, second) = (state.clone(), second.clone());
            std::thread::spawn(move || save_iptc(&state, id, &second))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!later.is_finished(), "the later save must wait for the earlier one's write");
        write(&original, &before, &first, turn).unwrap();
        later.join().unwrap().unwrap();

        let xml = read(&xmp);
        let expected = with(with(foreign_iptc(), "dc:title", &["t2"]), "dc:creator", &["c"]);
        assert_eq!(iptc(&xml), expected, "the sidecar must end on the newest save:\n{xml}");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, second);
    }

    /// Issue #149 F1: the library's volume goes away while a save waits for its turn. The
    /// save fails, the catalog row is unchanged (the store runs only once the turn is held),
    /// nothing is created where the volume was mounted, and the sidecar on the volume is
    /// untouched.
    #[test]
    fn a_volume_that_vanishes_while_a_save_waits_fails_the_save_and_changes_nothing() {
        let (dir, state, id, _xmp) = foreign_photo("iptc-149-unmount", crate::xmp::test_fixtures::LIGHTROOM);
        let library = dir.join("library");
        let earlier = crate::xmp::lock::WriteOrder::reserve(&library.join("DSC144.ARW"));
        let state = std::sync::Arc::new(state);
        let save = {
            let state = state.clone();
            std::thread::spawn(move || {
                save_iptc(&state, id, &IptcFields { title: "T".into(), ..Default::default() })
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!save.is_finished(), "the save must wait behind the earlier write");
        let unmounted = dir.join("unmounted");
        std::fs::rename(&library, &unmounted).unwrap();
        drop(earlier);

        let err = save.join().unwrap().unwrap_err();
        assert!(err.contains("no reachable copy"), "{err}");
        assert!(!library.exists(), "the save recreated the mount point's folder");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, IptcFields::default(), "a failed save must not change the catalog");
        assert_eq!(read(&crate::xmp::sidecar_path(&unmounted.join("DSC144.ARW"))),
            crate::xmp::test_fixtures::LIGHTROOM);
    }
}
