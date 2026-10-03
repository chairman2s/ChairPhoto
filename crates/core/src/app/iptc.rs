//! Saving a photo's authored IPTC fields: the catalog, then the photo's XMP sidecar.
//!
//! Shared by the Tauri `set_iptc` command and the GPUI inspector (gpui #108).

use super::{AppState, CatalogIdentity};
use crate::catalog::{IptcFields, IptcSettled, IptcSidecarState, IptcSidecarWrite};
use crate::xmp::lock::WriteOrder;

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
/// Overlapping saves of one photo store and write in one order (issue #149): a save first
/// takes the sidecar's write turn ([`WriteOrder`], reserved under the catalog lock, waited
/// for with none held) and stores, writes and settles while it holds it, so a later save
/// never has its change overwritten by an earlier one that reached the disk last. Storing
/// only once the turn is held also means a volume that went away while the save waited
/// fails the store, leaving the catalog unchanged, rather than the sidecar write.
///
/// The save is bound to the catalog it reserved in: a switch while it waits fails the
/// store closed with `CATALOG_CHANGED` instead of storing into the new catalog's row.
///
/// Blocking: lock order turn → catalog → file lock. The catalog lock is held for the path
/// lookup and the store (one hold, so the fields owed are the change stored), released for
/// the sidecar's read-modify-write, and taken again for the compare-and-set that records
/// it. Call it off the UI thread.
pub fn save_iptc(state: &AppState, photo_id: i64, fields: &IptcFields) -> Result<IptcSaveOutcome, String> {
    let (identity, turn) = super::with_catalog_identified(state, |c| reserve(c, photo_id))?;
    save_in_turn(state, identity, turn, photo_id, fields)
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
    let turn = super::with_catalog_as(state, expected, |c| reserve(c, photo_id))?;
    save_in_turn(state, expected, turn, photo_id, fields)
}

/// Wait for `turn` (no lock held), then store, write and settle in catalog `identity` while
/// holding it.
fn save_in_turn(
    state: &AppState,
    identity: CatalogIdentity,
    turn: WriteOrder,
    photo_id: i64,
    fields: &IptcFields,
) -> Result<IptcSaveOutcome, String> {
    let ((original, write), turn) = store_in_turn(state, identity, photo_id, turn, |c, original| {
        Ok((original, c.set_iptc(photo_id, fields)?))
    })?;
    Ok(write_and_settle(state, identity, &original, &write, turn))
}

/// How many times a store follows its photo to another sidecar before it gives up.
const MAX_MOVES: usize = 3;

/// Refusal of a store whose photo kept resolving to another copy while it waited: nothing
/// was stored, and saving again is safe.
pub const LOCATION_CHANGED: &str = "The photo's reachable copy changed while saving; nothing was saved, save again";

/// Wait for `turn` (no lock held), then — under catalog `identity`'s lock, with the turn
/// held — resolve the photo's original and run `store` with it. Returns `store`'s value
/// and the turn, to hold across the sidecar write. The path is resolved before `store`
/// writes anything, so an original that is unreachable now — gone while this waited for
/// its turn included — fails here and leaves the catalog unchanged.
///
/// The turn is keyed by the sidecar the original resolved to when it was reserved. When
/// the original now resolves to another sidecar — a photo with two locations whose
/// preferred copy came back while this waited — nothing is stored under it (#155 R1):
/// storing and writing under the old turn would order this write against the wrong
/// sidecar's writers, so two overlapping saves could each hold a turn while writing the
/// same file and the older value land last. Instead the turn follows the photo
/// ([`run_in_turn`]).
///
/// Blocking: call it on a blocking thread, never an async worker (see `xmp::lock`).
pub(crate) fn store_in_turn<T>(
    state: &AppState,
    identity: CatalogIdentity,
    photo_id: i64,
    turn: WriteOrder,
    mut store: impl FnMut(&crate::catalog::Catalog, std::path::PathBuf) -> crate::catalog::Result<T>,
) -> Result<(T, WriteOrder), String> {
    run_in_turn(state, identity, turn, |c, check| {
        let original = c.require_photo_path(photo_id)?;
        if !check.holds_for(&original) {
            return Ok(InTurn::Moved);
        }
        Ok(InTurn::Done(store(c, original)?))
    })
}

/// What one try under a write turn did: its result, or — the photo's original having
/// resolved to another sidecar than the turn's ([`TurnCheck::holds_for`] said so) — nothing.
pub(crate) enum InTurn<T> {
    Done(T),
    Moved,
}

/// The turn check handed to a [`run_in_turn`] step: whether a freshly resolved original
/// still maps to the sidecar the turn is for.
pub(crate) struct TurnCheck<'a> {
    turn: &'a WriteOrder,
    moved: Option<WriteOrder>,
}

impl TurnCheck<'_> {
    /// `true` while `original` maps to the turn's sidecar. Otherwise the new sidecar's turn
    /// is reserved (never blocks) and `false` returned: the step must then store and write
    /// nothing and answer [`InTurn::Moved`].
    pub(crate) fn holds_for(&mut self, original: &std::path::Path) -> bool {
        match self.turn.moved_to(original) {
            None => true,
            Some(next) => {
                self.moved = Some(next);
                false
            }
        }
    }
}

/// Wait for `turn` (no lock held), then run `step` under catalog `identity`'s lock with it
/// held, returning `step`'s value and the turn to hold across the sidecar write. A step that
/// resolves the original checks it with [`TurnCheck::holds_for`] before it stores or reads
/// what to write (#155 R1); when it answers [`InTurn::Moved`], this turn is released, the new
/// sidecar's turn (reserved under the lock) waited for with no lock held, and the step run
/// again. After [`MAX_MOVES`] moves it fails with [`LOCATION_CHANGED`], nothing stored.
///
/// Used by the IPTC save and the geocoder's fill ([`store_in_turn`]) and by the debt panel's
/// Retry (`app::iptc_owed`), which writes what is owed without storing.
///
/// Blocking: call it on a blocking thread, never an async worker (see `xmp::lock`).
pub(crate) fn run_in_turn<T>(
    state: &AppState,
    identity: CatalogIdentity,
    turn: WriteOrder,
    mut step: impl FnMut(&crate::catalog::Catalog, &mut TurnCheck<'_>) -> crate::catalog::Result<InTurn<T>>,
) -> Result<(T, WriteOrder), String> {
    let mut turn = turn.wait();
    for _ in 0..=MAX_MOVES {
        let (tried, moved) = super::with_catalog_as(state, identity, |c| {
            let mut check = TurnCheck { turn: &turn, moved: None };
            let tried = step(c, &mut check)?;
            Ok((tried, check.moved))
        })?;
        match (tried, moved) {
            (InTurn::Done(value), _) => return Ok((value, turn)),
            (InTurn::Moved, Some(next)) => {
                drop(turn);
                turn = next.wait();
            }
            (InTurn::Moved, None) => return Err("internal error: a write step moved without a new turn".into()),
        }
    }
    Err(LOCATION_CHANGED.into())
}

/// Reserve the place in line of the photo's sidecar write — under the catalog lock, which
/// never waits on a turn. An unreachable original fails here, before anything is stored.
fn reserve(c: &crate::catalog::Catalog, photo_id: i64) -> crate::catalog::Result<WriteOrder> {
    Ok(WriteOrder::reserve(&c.require_photo_path(photo_id)?))
}

/// Do `write`'s sidecar IO off the catalog lock with the write turn held, then record it in
/// the catalog that stored it, and only then let the next writer in line go. A switch in
/// between leaves the fields owed in that catalog, for its next repair pass.
pub(crate) fn write_and_settle(
    state: &AppState,
    identity: CatalogIdentity,
    original: &std::path::Path,
    write: &IptcSidecarWrite,
    turn: WriteOrder,
) -> IptcSaveOutcome {
    let outcome = write.run(original);
    #[cfg(test)]
    tests::before_settle(original);
    let settled = super::with_catalog_as(state, identity, |c| c.settle_iptc_write(write, &outcome));
    drop(turn);
    match settled {
        Ok(settled) => IptcSaveOutcome::from_settled(settled),
        Err(_) if write.fields.is_empty() => IptcSaveOutcome { sidecar: IptcSidecarState::Unchanged, reason: None },
        Err(e) => IptcSaveOutcome {
            sidecar: IptcSidecarState::Pending,
            reason: Some(format!("could not record the sidecar write: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::test_fixtures::{assert_non_iptc_intact, foreign_iptc, iptc, with, FOREIGN};

    /// A save's store without its turn check: the tests below that drive `write_and_settle`
    /// by hand use it to force landings a save in turn never makes.
    fn store(
        c: &crate::catalog::Catalog,
        photo_id: i64,
        fields: &IptcFields,
    ) -> crate::catalog::Result<(std::path::PathBuf, IptcSidecarWrite)> {
        let original = c.require_photo_path(photo_id)?;
        let write = c.set_iptc(photo_id, fields)?;
        Ok((original, write))
    }

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

    /// A hook `write_and_settle` runs between the sidecar write and the settle, once, for
    /// the write to the original it names — so a test can act inside that window.
    type Hook = (std::path::PathBuf, Box<dyn FnOnce() + Send>);
    static BEFORE_SETTLE: std::sync::Mutex<Option<Hook>> = std::sync::Mutex::new(None);

    pub(super) fn before_settle(original: &std::path::Path) {
        let hook = {
            let mut slot = BEFORE_SETTLE.lock().unwrap();
            if slot.as_ref().is_some_and(|(p, _)| p == original) { slot.take() } else { None }
        };
        if let Some((_, hook)) = hook {
            hook();
        }
    }

    /// The write turn is held through the settle, not only the write: a later save that
    /// arrives between an earlier save's write and its settle must not store until the
    /// settle is done. Otherwise its store bumps the generation first, and the earlier
    /// save's compare-and-set reports its landed write as superseded (pending) for no reason.
    #[test]
    fn the_turn_is_held_until_the_write_is_settled() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-148-hold", crate::xmp::test_fixtures::LIGHTROOM);
        let state = std::sync::Arc::new(state);
        let original = state.catalog.lock().unwrap().as_ref().unwrap().require_photo_path(id).unwrap();
        let first = IptcFields { title: "t1".into(), ..Default::default() };
        let second = IptcFields { title: "t2".into(), ..Default::default() };
        let (tx, rx) = std::sync::mpsc::channel();
        *BEFORE_SETTLE.lock().unwrap() = Some((original, Box::new({
            let state = state.clone();
            move || {
                let later = {
                    let state = state.clone();
                    std::thread::spawn(move || save_iptc(&state, id, &second))
                };
                std::thread::sleep(std::time::Duration::from_millis(200));
                let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
                tx.send((later, stored.title)).unwrap();
            }
        })));

        let earlier = save_iptc(&state, id, &first).unwrap();
        let (later, title_inside_the_window) = rx.recv().unwrap();
        assert_eq!(title_inside_the_window, "t1", "the later save stored before the earlier one settled");
        assert_eq!(earlier.sidecar, crate::catalog::IptcSidecarState::Written, "{earlier:?}");
        let later = later.join().unwrap().unwrap();
        assert_eq!(later.sidecar, crate::catalog::IptcSidecarState::Written, "{later:?}");
        assert!(iptc(&read(&xmp)).contains(&("dc:title".into(), vec!["t2".into()])));
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

    /// A write turn, waited for: what a save holds around its store and write. The tests
    /// below that drive `write_and_settle` by hand reserve it only for the write, so they
    /// can force the out-of-order landings the repair pass and bundle import (which take no
    /// turn) can still produce.
    fn turn(original: &std::path::Path) -> WriteOrder {
        WriteOrder::reserve(original).wait()
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

        let outcome = write_and_settle(&state, identity, &original, &write, turn(&original));
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

        let outcome = write_and_settle(&state, identity, &original, &a, turn(&original));
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Pending, "{outcome:?}");
        assert!(iptc(&read(&xmp)).contains(&("dc:title".into(), vec!["Older".into()])));
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE | crate::catalog::IptcMask::CREATOR);

        // The newer save's own write lands; it was read before the stale write re-owed, so it
        // too reports pending rather than claiming the sidecar is settled.
        let outcome = write_and_settle(&state, identity, &original, &b, turn(&original));
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

        let outcome = write_and_settle(&state, identity, &original, &b, turn(&original));
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Written, "{outcome:?}");
        write_and_settle(&state, identity, &original, &a, turn(&original));
        assert!(iptc(&read(&xmp)).contains(&("dc:title".into(), vec!["Older".into()])), "the stale bytes landed last");
        assert_eq!(owed(&state, id), crate::catalog::IptcMask::TITLE, "so the title is owed again");

        repair(&state);
        assert_eq!(iptc(&read(&xmp)), with(foreign_iptc(), "dc:title", &["Newer"]));
    }

    /// Issue #149, probe P2: two overlapping saves of one photo. The first holds its turn,
    /// between storing t1 and writing it; a second (t2 and a creator) arrives. The second
    /// must wait for the first's write, so the sidecar ends where the catalog does — t2 —
    /// instead of the first save's late write putting t1 back.
    #[test]
    fn overlapping_saves_write_the_sidecar_in_the_order_they_stored() {
        let (_dir, state, id, xmp) = foreign_photo("iptc-149-order", crate::xmp::test_fixtures::LIGHTROOM);
        let state = std::sync::Arc::new(state);
        let identity = crate::app::catalog_identity(&state).unwrap();
        let first = IptcFields { title: "t1".into(), ..Default::default() };
        let turn = crate::app::with_catalog_as(&state, identity, |c| reserve(c, id)).unwrap().wait();
        let (original, write) = crate::app::with_catalog_as(&state, identity, |c| store(c, id, &first)).unwrap();

        let second = IptcFields { title: "t2".into(), creator: "c".into(), ..Default::default() };
        let later = {
            let (state, second) = (state.clone(), second.clone());
            std::thread::spawn(move || save_iptc(&state, id, &second))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!later.is_finished(), "the later save must wait for the earlier one's write");
        let earlier = write_and_settle(&state, identity, &original, &write, turn);
        // The later save had not stored yet, so the earlier one's compare-and-set holds.
        assert_eq!(earlier.sidecar, crate::catalog::IptcSidecarState::Written, "{earlier:?}");
        let later = later.join().unwrap().unwrap();
        assert_eq!(later.sidecar, crate::catalog::IptcSidecarState::Written, "{later:?}");

        let xml = read(&xmp);
        let expected = with(with(foreign_iptc(), "dc:title", &["t2"]), "dc:creator", &["c"]);
        assert_eq!(iptc(&xml), expected, "the sidecar must end on the newest save:\n{xml}");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, second);
        assert_eq!(state.catalog.lock().unwrap().as_ref().unwrap().owed_iptc(id).unwrap(), crate::catalog::IptcMask::NONE);
    }

    /// #155 R1: a photo with two copies — the primary in the library, a backup on a NAS —
    /// whose primary comes back while a save waits. The save reserved the NAS sidecar's turn
    /// (the primary was away), but at its store the original resolves to the library again.
    /// It must not store and write the library sidecar under the NAS turn while another
    /// writer holds the library sidecar's: it gives the NAS turn up, waits for the library
    /// one, and only then stores and writes there. The NAS sidecar is never touched.
    #[test]
    fn a_save_whose_copy_changes_while_it_waits_takes_the_new_sidecars_turn() {
        let (dir, state, id, library_xmp) = foreign_photo("iptc-155-moved", crate::xmp::test_fixtures::LIGHTROOM);
        let library = dir.join("library");
        let nas = dir.join("nas");
        std::fs::create_dir_all(&nas).unwrap();
        let nas_file = nas.join("DSC144.ARW");
        std::fs::write(&nas_file, b"raw").unwrap();
        std::fs::write(crate::xmp::sidecar_path(&nas_file), crate::xmp::test_fixtures::LIGHTROOM).unwrap();
        {
            let guard = state.catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            let volume = c.add_volume("NAS", &nas, crate::catalog::VolumeKind::Backup).unwrap();
            c.add_location(id, volume, "DSC144.ARW", crate::catalog::LocationRole::Backup).unwrap();
        }

        // The primary is away: the save resolves the NAS copy and waits behind a write that
        // holds that sidecar's turn.
        let away = dir.join("library.away");
        std::fs::rename(&library, &away).unwrap();
        let nas_writer = WriteOrder::reserve(&nas_file);
        let state = std::sync::Arc::new(state);
        let save = {
            let state = state.clone();
            std::thread::spawn(move || save_iptc(&state, id, &IptcFields { title: "t1".into(), ..Default::default() }))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!save.is_finished(), "the save must wait behind the NAS sidecar's writer");

        // The primary is back, and another writer holds the library sidecar's turn.
        std::fs::rename(&away, &library).unwrap();
        let library_writer = WriteOrder::reserve(&library.join("DSC144.ARW")).wait();
        drop(nas_writer);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!save.is_finished(), "the save wrote the library sidecar under the NAS sidecar's turn");
        let stored = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!(stored, IptcFields::default(), "the save stored before it held the library sidecar's turn");

        drop(library_writer);
        let outcome = save.join().unwrap().unwrap();
        assert_eq!(outcome.sidecar, crate::catalog::IptcSidecarState::Written, "{outcome:?}");
        assert_eq!(iptc(&read(&library_xmp)), with(foreign_iptc(), "dc:title", &["t1"]));
        assert_eq!(read(&crate::xmp::sidecar_path(&nas_file)), crate::xmp::test_fixtures::LIGHTROOM);
    }

    /// #155 R2: the unbound save (the Tauri `set_iptc`, which names no catalog) is bound to
    /// the catalog it reserved its turn in. A switch while it waits — the window #149 opened
    /// between the reserve and the store — fails it closed with `CATALOG_CHANGED`: the new
    /// catalog's row with the same id is not stored into, and neither catalog's sidecar is
    /// written.
    #[test]
    fn an_unbound_save_waiting_through_a_catalog_switch_changes_neither_catalog() {
        let (dir, state, id, xmp) = foreign_photo("iptc-155-switch", crate::xmp::test_fixtures::LIGHTROOM);
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let other_file = other.join("IMG_B.ARW");
        std::fs::write(&other_file, b"another catalog's photo").unwrap();
        let other_xmp = crate::xmp::sidecar_path(&other_file);
        std::fs::write(&other_xmp, crate::xmp::test_fixtures::LIGHTROOM).unwrap();
        let b = crate::catalog::Catalog::open(&dir.join("b.chairphoto"), &other).unwrap();
        assert_eq!(b.upsert_photo(&other_file, None, 0, 1).unwrap().id, id, "the ids collide");

        let earlier = WriteOrder::reserve(&dir.join("library").join("DSC144.ARW"));
        let state = std::sync::Arc::new(state);
        let save = {
            let state = state.clone();
            std::thread::spawn(move || save_iptc(&state, id, &IptcFields { title: "T".into(), ..Default::default() }))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!save.is_finished(), "the save must wait behind the earlier write");
        let a = state.catalog.lock().unwrap().replace(b).unwrap();
        drop(earlier);

        assert_eq!(save.join().unwrap().unwrap_err(), crate::app::CATALOG_CHANGED);
        let guard = state.catalog.lock().unwrap();
        let b = guard.as_ref().unwrap();
        assert_eq!(b.get_iptc(id).unwrap(), IptcFields::default(), "the new catalog's row was stored into");
        assert_eq!(b.owed_iptc(id).unwrap(), crate::catalog::IptcMask::NONE);
        assert_eq!(a.get_iptc(id).unwrap(), IptcFields::default(), "the old catalog's row was stored into");
        assert_eq!(read(&xmp), crate::xmp::test_fixtures::LIGHTROOM);
        assert_eq!(read(&other_xmp), crate::xmp::test_fixtures::LIGHTROOM);
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
        let owed = state.catalog.lock().unwrap().as_ref().unwrap().owed_iptc(id).unwrap();
        assert_eq!(owed, crate::catalog::IptcMask::NONE, "nor owe the sidecar anything");
        assert_eq!(read(&crate::xmp::sidecar_path(&unmounted.join("DSC144.ARW"))),
            crate::xmp::test_fixtures::LIGHTROOM);
    }
}
