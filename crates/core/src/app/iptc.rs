//! Saving a photo's authored IPTC fields: the catalog, then the photo's XMP sidecar.
//!
//! Shared by the Tauri `set_iptc` command and the GPUI inspector (gpui #108).

use super::{AppState, CatalogIdentity};
use crate::catalog::{IptcFields, IptcSettled, IptcSidecarState, IptcSidecarWrite};

/// What an IPTC save did. The catalog half always succeeded (a failure there is the save's
/// `Err`); `sidecar` says whether the sidecar has caught up.
///
/// The Tauri `set_iptc` command returns this where it used to return nothing, so a caller
/// that ignored the result still works.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IptcSaveOutcome {
    /// `written`: the sidecar now has every value the catalog holds. `unchanged`: nothing
    /// was owed, so the sidecar was not opened. `pending`: saved to the catalog only — the
    /// fields stay owed (#148) and the next save or the repair pass writes them.
    pub sidecar: IptcSidecarState,
    /// Why the sidecar is pending, when it is.
    pub reason: Option<String>,
}

impl IptcSaveOutcome {
    fn from_settled(settled: IptcSettled) -> Self {
        let reason = match &settled {
            IptcSettled::Failed(e) => Some(e.clone()),
            IptcSettled::Superseded => Some("a newer change to this photo's IPTC is being written".into()),
            IptcSettled::Written | IptcSettled::Unchanged => None,
        };
        IptcSaveOutcome { sidecar: settled.state(), reason }
    }

    /// The status line a front end shows for it. Never claims a sidecar write that did not
    /// happen.
    pub fn status(&self) -> String {
        match (self.sidecar, &self.reason) {
            (IptcSidecarState::Written, _) => "Saved to sidecar".into(),
            // Nothing was owed, so the sidecar was not opened: say that, not that it holds
            // the values — it was never read (review of #148, N3).
            (IptcSidecarState::Unchanged, _) => "Saved (no sidecar change needed)".into(),
            (IptcSidecarState::Pending, Some(why)) => format!("Saved to catalog; sidecar pending ({why})"),
            (IptcSidecarState::Pending, None) => "Saved to catalog; sidecar pending".into(),
        }
    }
}

/// Store `fields` in the catalog, then write the photo's owed IPTC to its XMP sidecar
/// (merge-safe: `xmp::write_iptc_fields` touches only the managed fields this save changed
/// plus any an earlier write left owed — #144, #148 — so a sidecar value ChairPhoto never
/// imported survives a save that left that field alone). The sidecar sits next to the
/// original the location resolver finds.
///
/// With no reachable copy the save fails closed: the error says why, and neither the
/// catalog nor the sidecar changes. Once the catalog has stored the values, a sidecar write
/// that fails (read-only storage, an unparseable sidecar, a volume gone mid-save) is not an
/// error: the fields stay owed and the outcome says `pending`. The next save of the photo or
/// the repair pass writes them.
///
/// Blocking: the catalog lock is held for the path lookup and the store (one hold, so the
/// fields owed are the change stored), released for the sidecar's read-modify-write, and
/// taken again for the compare-and-set that records it. Call it off the UI thread.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<IptcSaveOutcome, String> {
    let (identity, (original, write)) = super::with_catalog_identified(state, |c| store(c, photo_id, fields))?;
    Ok(write_and_settle(state, identity, &original, &write))
}

/// [`save_iptc`] of a photo read from the catalog `expected` names: the store and the path
/// lookup fail closed with `CATALOG_CHANGED` once another catalog is open, so neither the
/// other catalog's row with the same id nor its photo's sidecar is written.
pub fn save_iptc_as(
    state: &AppState,
    expected: CatalogIdentity,
    photo_id: i64,
    fields: &IptcFields,
) -> Result<IptcSaveOutcome, String> {
    let (original, write) = super::with_catalog_as(state, expected, |c| store(c, photo_id, fields))?;
    Ok(write_and_settle(state, expected, &original, &write))
}

/// Do `write`'s sidecar IO off the lock, then record it in the catalog that stored it. A
/// switch in between leaves the fields owed in that catalog, for its next repair pass.
pub(crate) fn write_and_settle(
    state: &AppState,
    identity: CatalogIdentity,
    original: &std::path::Path,
    write: &IptcSidecarWrite,
) -> IptcSaveOutcome {
    let outcome = write.run(original);
    match super::with_catalog_as(state, identity, |c| c.settle_iptc_write(write, &outcome)) {
        Ok(settled) => IptcSaveOutcome::from_settled(settled),
        Err(_) if write.fields.is_empty() => IptcSaveOutcome { sidecar: IptcSidecarState::Unchanged, reason: None },
        Err(e) => IptcSaveOutcome {
            sidecar: IptcSidecarState::Pending,
            reason: Some(format!("could not record the sidecar write: {e}")),
        },
    }
}

/// Store `fields`, returning the original's path and the sidecar write the store owes. The
/// path is resolved before the row is written (as the geocoder's `fill_in` does), so an
/// unreachable original leaves the catalog unchanged.
fn store(
    c: &crate::catalog::Catalog,
    photo_id: i64,
    fields: &IptcFields,
) -> crate::catalog::Result<(std::path::PathBuf, IptcSidecarWrite)> {
    let original = c.require_photo_path(photo_id)?;
    let write = c.set_iptc(photo_id, fields)?;
    Ok((original, write))
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

    // ── #148: a sidecar write that fails after the store is owed, and retried ─────────

    fn owed(state: &AppState, id: i64) -> crate::catalog::IptcMask {
        state.catalog.lock().unwrap().as_ref().unwrap().owed_iptc(id).unwrap()
    }

    fn repair(state: &AppState) -> crate::catalog::IdentityRepairSummary {
        state.catalog.lock().unwrap().as_ref().unwrap().repair_pending_identity().unwrap()
    }

    fn typed() -> IptcFields {
        IptcFields { title: "Mine".into(), creator: "Me".into(), ..Default::default() }
    }

    /// The Lightroom fixture's IPTC once `typed()` has reached it.
    fn written() -> Vec<(String, Vec<String>)> {
        with(with(foreign_iptc(), "dc:title", &["Mine"]), "dc:creator", &["Me"])
    }

    #[cfg(unix)]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Read-only storage: the save reaches the catalog, reports the sidecar pending (never
    /// "Saved to sidecar"), and owes both fields. Once the volume is writable the repair pass
    /// writes them and nothing is owed.
    #[cfg(unix)]
    #[test]
    fn a_read_only_volume_leaves_the_fields_owed_and_the_pass_writes_them() {
        let (dir, state, id, xmp) = foreign_photo("iptc-148-ro", crate::xmp::test_fixtures::LIGHTROOM);
        let library = dir.join("library");
        set_mode(&xmp, 0o444);
        set_mode(&library, 0o555);
        if std::fs::write(library.join("probe"), b"").is_ok() {
            set_mode(&library, 0o755);
            println!("SKIPPED: a_read_only_volume_leaves_the_fields_owed_and_the_pass_writes_them — running with permission to write a read-only directory (root?)");
            return;
        }

        let outcome = save_iptc(&state, id, &typed()).unwrap();
        set_mode(&library, 0o755);
        set_mode(&xmp, 0o644);
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert!(outcome.status().starts_with("Saved to catalog; sidecar pending"), "{}", outcome.status());
        assert_eq!(read(&xmp), crate::xmp::test_fixtures::LIGHTROOM, "nothing reached the sidecar");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);

        let summary = repair(&state);
        assert_eq!((summary.iptc_written, summary.iptc_failed, summary.total), (1, 0, 1), "{summary:?}");
        let xml = read(&xmp);
        assert_eq!(iptc(&xml), written(), "the pass wrote the owed fields:\n{xml}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::NONE);
    }

    /// An unparseable sidecar: pending and owed. Once the file parses again, a re-save of
    /// the same values (the retry a user makes) writes them — it is no longer a no-op.
    #[test]
    fn an_unparseable_sidecar_leaves_the_fields_owed_and_a_re_save_writes_them() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-148-garbage", "<x:xmpmeta this is not xml");
        let outcome = save_iptc(&state, id, &typed()).unwrap();
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert!(outcome.reason.is_some());
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, typed(), "the catalog has the values");

        // A pass while the file is still broken leaves the fields owed and says so.
        let summary = repair(&state);
        assert_eq!((summary.iptc_written, summary.iptc_failed), (0, 1), "{summary:?}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);

        std::fs::write(&xmp, crate::xmp::test_fixtures::LIGHTROOM).unwrap();
        let outcome = save_iptc(&state, id, &typed()).unwrap();
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Written, "{outcome:?}");
        let xml = read(&xmp);
        assert_eq!(iptc(&xml), written(), "the re-save wrote the owed fields:\n{xml}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::NONE);

        // With nothing owed, a further identical save writes nothing and says so.
        let outcome = save_iptc(&state, id, &typed()).unwrap();
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Unchanged);
        assert_eq!(outcome.status(), "Saved (no sidecar change needed)");
    }

    /// The volume unmounts between the store and the sidecar write: the fields stay owed,
    /// and once it is back the repair pass writes them. A pass while it is away counts the
    /// photo unreachable and leaves it owed.
    #[test]
    fn an_unmount_mid_save_leaves_the_fields_owed_and_the_pass_writes_them_once_mounted() {
        let (dir, state, id, xmp) = foreign_photo("iptc-148-unmount", crate::xmp::test_fixtures::LIGHTROOM);
        let identity = crate::app::catalog_identity(&state).unwrap();
        let (original, write) =
            crate::app::with_catalog_as(&state, identity, |c| store(c, id, &typed())).unwrap();
        let library = dir.join("library");
        let away = dir.join("library.away");
        std::fs::rename(&library, &away).unwrap();

        let outcome = write_and_settle(&state, identity, &original, &write);
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);
        let summary = repair(&state);
        assert_eq!((summary.iptc_written, summary.iptc_unreachable), (0, 1), "{summary:?}");

        std::fs::rename(&away, &library).unwrap();
        let summary = repair(&state);
        assert_eq!(summary.iptc_written, 1, "{summary:?}");
        let xml = read(&xmp);
        assert_eq!(iptc(&xml), written(), "{xml}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::NONE);
    }

    /// Two saves overlap: the older one's write runs after the newer one's store. Its
    /// compare-and-set must not clear the newer save's debt — the sidecar it wrote holds the
    /// older title — and the next write puts the catalog's values on disk.
    #[test]
    fn a_stale_save_does_not_clear_a_newer_saves_debt() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-148-cas", crate::xmp::test_fixtures::LIGHTROOM);
        let identity = crate::app::catalog_identity(&state).unwrap();
        let older = IptcFields { title: "Older".into(), ..Default::default() };
        let newer = IptcFields { title: "Newer".into(), creator: "Me".into(), ..Default::default() };
        let (original, a) = crate::app::with_catalog_as(&state, identity, |c| store(c, id, &older)).unwrap();
        let (_, b) = crate::app::with_catalog_as(&state, identity, |c| store(c, id, &newer)).unwrap();

        let outcome = write_and_settle(&state, identity, &original, &a);
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert!(iptc(&read(&xmp)).contains(&("dc:title".into(), vec!["Older".into()])));
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);

        // The newer save's own write lands; it was read before the stale write re-owed, so it
        // too reports pending rather than claiming the sidecar is settled.
        let outcome = write_and_settle(&state, identity, &original, &b);
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert_eq!(repair(&state).iptc_written, 1);
        let xml = read(&xmp);
        let expected = with(with(foreign_iptc(), "dc:title", &["Newer"]), "dc:creator", &["Me"]);
        assert_eq!(iptc(&xml), expected, "{xml}");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::NONE);
    }

    /// The newer save writes and settles first, then the older save's bytes land: they are
    /// the last on disk, so the older save owes its fields again and the pass restores the
    /// catalog's values.
    #[test]
    fn a_stale_write_landing_after_the_newer_one_is_repaired() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-148-late", crate::xmp::test_fixtures::LIGHTROOM);
        let identity = crate::app::catalog_identity(&state).unwrap();
        let older = IptcFields { title: "Older".into(), ..Default::default() };
        let newer = IptcFields { title: "Newer".into(), ..Default::default() };
        let (original, a) = crate::app::with_catalog_as(&state, identity, |c| store(c, id, &older)).unwrap();
        let (_, b) = crate::app::with_catalog_as(&state, identity, |c| store(c, id, &newer)).unwrap();

        let outcome = write_and_settle(&state, identity, &original, &b);
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Written, "{outcome:?}");
        write_and_settle(&state, identity, &original, &a);
        assert!(iptc(&read(&xmp)).contains(&("dc:title".into(), vec!["Older".into()])), "the stale bytes landed last");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE, "so the title is owed again");

        repair(&state);
        assert_eq!(iptc(&read(&xmp)), with(foreign_iptc(), "dc:title", &["Newer"]));
    }
}
