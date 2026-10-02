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
/// Overlapping saves of one photo write its sidecar in the order they stored (issue #149):
/// the store reserves the sidecar write's place in line ([`crate::xmp::lock::WriteOrder`])
/// under the catalog lock, so a later save never has its change overwritten by an earlier
/// one that reached the disk last.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<(), String> {
    let (original, before, order) = with_catalog(state, |c| store(c, photo_id, fields))?;
    write(&original, &before, fields, order)
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
    let (original, before, order) = super::with_catalog_as(state, expected, |c| store(c, photo_id, fields))?;
    write(&original, &before, fields, order)
}

/// Store `fields`, returning the original's path, the values they replaced and the sidecar
/// write's place in line, reserved here — under the catalog lock the store holds — so the
/// line's order is the store order. The path is resolved before the row is written (as the
/// geocoder's `fill_in` does), so an unreachable original leaves the catalog unchanged.
fn store(
    c: &crate::catalog::Catalog,
    photo_id: i64,
    fields: &IptcFields,
) -> crate::catalog::Result<(std::path::PathBuf, IptcFields, crate::xmp::lock::WriteOrder)> {
    let original = c.require_photo_path(photo_id)?;
    let before = c.get_iptc(photo_id)?;
    c.set_iptc(photo_id, fields)?;
    let order = crate::xmp::lock::WriteOrder::reserve(&original);
    Ok((original, before, order))
}

/// The sidecar half of a save, off the catalog lock: wait for every earlier save's write,
/// then write this one's change.
fn write(
    original: &std::path::Path,
    before: &IptcFields,
    after: &IptcFields,
    order: crate::xmp::lock::WriteOrder,
) -> Result<(), String> {
    let _turn = order.wait();
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

    /// Issue #149, probe P2: two overlapping saves of one photo. The first stores t1; before
    /// its sidecar write runs, a second stores t2 and a creator and goes to write. The second
    /// must wait for the first, so the sidecar ends where the catalog does — t2 — instead of
    /// the first save's late write putting t1 back.
    #[test]
    fn overlapping_saves_write_the_sidecar_in_the_order_they_stored() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-149-order", crate::xmp::test_fixtures::LIGHTROOM);
        let state = std::sync::Arc::new(state);
        let first = IptcFields { title: "t1".into(), ..Default::default() };
        let (original, before, order) = with_catalog(&state, |c| store(c, id, &first)).unwrap();

        let second = IptcFields { title: "t2".into(), creator: "c".into(), ..Default::default() };
        let later = {
            let (state, second) = (state.clone(), second.clone());
            std::thread::spawn(move || save_iptc(&state, id, &second))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!later.is_finished(), "the later save must wait for the earlier one's write");
        write(&original, &before, &first, order).unwrap();
        later.join().unwrap().unwrap();

        let xml = read(&xmp);
        let expected = with(with(foreign_iptc(), "dc:title", &["t2"]), "dc:creator", &["c"]);
        assert_eq!(iptc(&xml), expected, "the sidecar must end on the newest save:\n{xml}");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, second);
    }
}
