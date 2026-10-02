//! Saving a photo's authored IPTC fields: the catalog, then the photo's XMP sidecar.
//!
//! Shared by the Tauri `set_iptc` command and the GPUI inspector (gpui #108).

use super::{with_catalog, AppState};
use crate::catalog::IptcFields;

/// Store `fields` in the catalog, then write the change to the photo's XMP sidecar
/// (merge-safe: `xmp::write_iptc` touches only the managed IPTC fields whose catalog value
/// this save changed, so a sidecar value ChairPhoto never imported survives a save that
/// left that field alone — issue #144). The sidecar sits next to the original the location
/// resolver finds; with no reachable copy the catalog keeps the values and the error says
/// why the sidecar was not written.
///
/// Blocking: the catalog lock is held only for the read of the previous values, the store
/// and the path lookup — one hold, so the change written is the change stored — and the
/// sidecar's read-modify-write runs after it is released. Call it off the UI thread.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<(), String> {
    let (original, before) = with_catalog(state, |c| store(c, photo_id, fields))?;
    crate::xmp::write_iptc(&original, &before, fields)
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
    let (original, before) = super::with_catalog_as(state, expected, |c| store(c, photo_id, fields))?;
    crate::xmp::write_iptc(&original, &before, fields)
}

/// Store `fields`, returning the original's path and the values they replaced.
fn store(
    c: &crate::catalog::Catalog,
    photo_id: i64,
    fields: &IptcFields,
) -> crate::catalog::Result<(std::path::PathBuf, IptcFields)> {
    let before = c.get_iptc(photo_id)?;
    c.set_iptc(photo_id, fields)?;
    Ok((c.require_photo_path(photo_id)?, before))
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
}
