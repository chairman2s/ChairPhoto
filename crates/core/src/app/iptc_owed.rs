//! The identity-debt panel's per-photo owed IPTC (#153): list, Dismiss, Retry one.
//!
//! Shared by the Tauri `list_owed_iptc` / `dismiss_owed_iptc` / `retry_owed_iptc` commands
//! and the GPUI identity-debt panel. The record and its compare-and-set are the catalog's
//! (`catalog/iptc_owed.rs`); this module binds each action to the catalog its row was read
//! from and runs Retry through the save's write turn and write+settle path.
//!
//! A row names its photo by id *and* UUID. GPUI also passes the [`CatalogIdentity`] its page
//! was read with, so an action after a catalog switch fails closed with
//! [`CATALOG_CHANGED`](super::CATALOG_CHANGED). The Tauri shell carries no identity (`None`);
//! there the UUID check is the guard against an id that names another photo now.

use super::iptc::{run_in_turn, write_and_settle, InTurn, IptcSaveOutcome};
use super::{AppState, CatalogIdentity};
use crate::catalog::{Catalog, IptcSidecarState, OwedIptc};
use crate::xmp::lock::WriteOrder;

/// What a Retry or Dismiss answers when the row's photo id no longer names the photo with
/// the row's UUID (removed, or the id taken by another photo).
pub const OWED_PHOTO_GONE: &str = "This photo is no longer in the catalog";

/// One page of the photos owing IPTC, with the identity of the catalog it was read from.
/// Blocking (SQLite): call it off the UI thread.
pub fn list_owed_iptc(state: &AppState, limit: i64, offset: i64) -> Result<(CatalogIdentity, Vec<OwedIptc>), String> {
    super::with_catalog_identified(state, |c| c.list_owed_iptc_page(limit, offset))
}

/// [`super::with_catalog_as`] when `expected` names a catalog, otherwise whatever catalog is
/// open — returning the identity either way, so a second step can be bound to the first.
fn with_bound<T>(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<(CatalogIdentity, T), String> {
    match expected {
        Some(identity) => super::with_catalog_as(state, identity, f).map(|t| (identity, t)),
        None => super::with_catalog_identified(state, f),
    }
}

/// Dismiss the row a front end showed: stop owing the photo's IPTC without writing
/// ([`Catalog::dismiss_owed_iptc`], compare-and-set on `uuid` + `generation`). `Ok(false)`
/// when a newer store owes something since the row was read, or the photo is gone: nothing
/// was dismissed, and the front end re-reads the list. Blocking (SQLite).
pub fn dismiss_owed_iptc_as(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_id: i64,
    uuid: &str,
    generation: i64,
) -> Result<bool, String> {
    with_bound(state, expected, |c| c.dismiss_owed_iptc(photo_id, uuid, generation)).map(|(_, done)| done)
}

/// Record that the photo has no reachable copy (as the repair pass does) and answer
/// pending. Under the catalog lock.
fn unreachable(c: &Catalog, photo_id: i64) -> crate::catalog::Result<IptcSaveOutcome> {
    let Some(write) = c.owed_iptc_write(photo_id)? else { return Ok(nothing_owed()) };
    let why = format!("no reachable copy of photo {photo_id}");
    c.settle_iptc_write(&write, &Err(why.clone()))?;
    Ok(IptcSaveOutcome { sidecar: IptcSidecarState::Pending, reason: Some(why) })
}

fn nothing_owed() -> IptcSaveOutcome {
    IptcSaveOutcome { sidecar: IptcSidecarState::Unchanged, reason: None }
}

/// The first step: the original's write turn, or an answer already.
enum Reserved {
    Turn(WriteOrder),
    Done(IptcSaveOutcome),
    Gone,
}

/// The step taken with the turn held.
enum Ready {
    Write(std::path::PathBuf, crate::catalog::IptcSidecarWrite),
    Done(IptcSaveOutcome),
    Gone,
}

/// Retry the row a front end showed: write the photo's owed IPTC into its sidecar now, as a
/// save would. Takes the sidecar's write turn ([`WriteOrder`], reserved under the catalog
/// lock and waited for with none held), reads what is owed *now* with that turn held — a
/// save since the row was read may have added to it or paid it — and writes and settles it
/// through the save's [`write_and_settle`] (compare-and-set on the generation it read).
///
/// Answers like a save: `written`, `unchanged` (nothing owed any more), or `pending` with
/// the reason — an unreachable original included, which is recorded on the row as the
/// repair pass records it, and stays owed. A photo id that no longer names `uuid` fails with
/// [`OWED_PHOTO_GONE`]; a catalog switch, when `expected` is given, with `CATALOG_CHANGED`.
///
/// Blocking (it waits for the write turn, and does sidecar IO): call it on a blocking
/// thread — `spawn_blocking` or the GPUI runner — never on an async runtime's worker.
pub fn retry_owed_iptc_as(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_id: i64,
    uuid: &str,
) -> Result<IptcSaveOutcome, String> {
    let (identity, reserved) = with_bound(state, expected, |c| {
        if !c.photo_has_uuid(photo_id, uuid)? {
            return Ok(Reserved::Gone);
        }
        Ok(match c.resolve_photo_path(photo_id)? {
            Some(original) => Reserved::Turn(WriteOrder::reserve(&original)),
            None => Reserved::Done(unreachable(c, photo_id)?),
        })
    })?;
    let turn = match reserved {
        Reserved::Turn(turn) => turn,
        Reserved::Done(outcome) => return Ok(outcome),
        Reserved::Gone => return Err(OWED_PHOTO_GONE.into()),
    };
    // With the turn held — the turn of the sidecar the original resolves to now, followed
    // there if the photo's reachable copy changed while Retry waited (#155 R1).
    let (ready, turn) = run_in_turn(state, identity, turn, |c, check| {
        if !c.photo_has_uuid(photo_id, uuid)? {
            return Ok(InTurn::Done(Ready::Gone));
        }
        let Some(original) = c.resolve_photo_path(photo_id)? else {
            return Ok(InTurn::Done(Ready::Done(unreachable(c, photo_id)?)));
        };
        if !check.holds_for(&original) {
            return Ok(InTurn::Moved);
        }
        let Some(write) = c.owed_iptc_write(photo_id)? else { return Ok(InTurn::Done(Ready::Done(nothing_owed()))) };
        Ok(InTurn::Done(Ready::Write(original, write)))
    })?;
    match ready {
        Ready::Write(original, write) => Ok(write_and_settle(state, identity, &original, &write, turn)),
        Ready::Done(outcome) => Ok(outcome),
        Ready::Gone => Err(OWED_PHOTO_GONE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{IptcFields, IptcMask};
    use crate::xmp::test_fixtures::{foreign_iptc, iptc, with, LIGHTROOM};

    /// A catalog with one photo whose sidecar is the Lightroom fixture, open in an AppState.
    fn photo(tag: &str) -> (crate::test_support::TestTmpDir, AppState, i64, std::path::PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("DSC153.ARW");
        std::fs::write(&file, b"raw").unwrap();
        std::fs::write(crate::xmp::sidecar_path(&file), LIGHTROOM).unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let id = catalog.upsert_photo(&file, None, 0, 1).unwrap().id;
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        (dir, state, id, file)
    }

    fn titled(t: &str) -> IptcFields {
        IptcFields { title: t.into(), ..Default::default() }
    }

    fn c<T>(state: &AppState, f: impl FnOnce(&Catalog) -> T) -> T {
        f(state.catalog.lock().unwrap().as_ref().unwrap())
    }

    /// Owe `fields` without writing them: the store a failed sidecar write leaves behind.
    fn owe(state: &AppState, id: i64, fields: &IptcFields) -> OwedIptc {
        c(state, |c| {
            let w = c.set_iptc(id, fields).unwrap();
            c.settle_iptc_write(&w, &Err("read-only".into())).unwrap();
            c.list_owed_iptc_page(10, 0).unwrap().into_iter().find(|r| r.photo_id == id).unwrap()
        })
    }

    /// Retry writes the owed fields through the save path and clears the debt; a second
    /// Retry finds nothing owed.
    #[test]
    fn retry_writes_the_owed_fields_and_clears_the_debt() {
        let (_dir, state, id, file) = photo("iptc-153-retry");
        let row = owe(&state, id, &IptcFields { creator: "Me".into(), ..titled("Mine") });
        assert_eq!(row.fields, ["Title", "Creator"]);
        assert_eq!(row.error, "read-only");

        let outcome = retry_owed_iptc_as(&state, None, id, &row.uuid).unwrap();
        assert_eq!(outcome.sidecar, IptcSidecarState::Written, "{outcome:?}");
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&file)).unwrap();
        assert_eq!(iptc(&xml), with(with(foreign_iptc(), "dc:title", &["Mine"]), "dc:creator", &["Me"]), "{xml}");
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::NONE);
        assert!(c(&state, |c| c.list_owed_iptc_page(10, 0).unwrap()).is_empty());

        let again = retry_owed_iptc_as(&state, None, id, &row.uuid).unwrap();
        assert_eq!(again.sidecar, IptcSidecarState::Unchanged);
    }

    /// An unreachable original: Retry answers pending, the debt stays, and the row records
    /// the attempt and why.
    #[test]
    fn retry_of_an_unreachable_photo_stays_owed_and_records_why() {
        let (dir, state, id, file) = photo("iptc-153-retry-away");
        let row = owe(&state, id, &titled("Mine"));
        std::fs::rename(&file, dir.join("away.ARW")).unwrap();

        let outcome = retry_owed_iptc_as(&state, None, id, &row.uuid).unwrap();
        assert_eq!(outcome.sidecar, IptcSidecarState::Pending, "{outcome:?}");
        assert!(outcome.reason.as_deref().unwrap().contains("no reachable copy"), "{outcome:?}");
        let after = c(&state, |c| c.list_owed_iptc_page(10, 0).unwrap());
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].attempts, row.attempts + 1);
        assert!(after[0].error.contains("no reachable copy"), "{:?}", after[0]);
    }

    /// Retry waits for the sidecar's write turn: while an earlier writer holds it, Retry
    /// neither writes nor settles; once released, it writes what is owed then.
    #[test]
    fn retry_waits_for_the_write_turn() {
        let (_dir, state, id, file) = photo("iptc-153-retry-turn");
        let row = owe(&state, id, &titled("Mine"));
        let earlier = WriteOrder::reserve(&file).wait();
        let state = std::sync::Arc::new(state);
        let retry = {
            let (state, uuid) = (state.clone(), row.uuid.clone());
            std::thread::spawn(move || retry_owed_iptc_as(&state, None, id, &uuid))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!retry.is_finished(), "Retry ran without the write turn");
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::TITLE);
        drop(earlier);
        let outcome = retry.join().unwrap().unwrap();
        assert_eq!(outcome.sidecar, IptcSidecarState::Written, "{outcome:?}");
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::NONE);
    }

    /// #155 R1 for Retry: a photo with a library primary and a NAS backup. Retry reserves the
    /// NAS sidecar's turn while the primary is away; the primary comes back while Retry
    /// waits, and another writer holds the library sidecar's turn. Retry must not write the
    /// library sidecar under the NAS turn: it follows the photo to the library sidecar's turn,
    /// writes once that is released, and never touches the NAS sidecar.
    #[test]
    fn retry_whose_copy_changes_while_it_waits_takes_the_new_sidecars_turn() {
        let (dir, state, id, file) = photo("iptc-155-retry-moved");
        let row = owe(&state, id, &titled("Mine"));
        let nas = dir.join("nas");
        std::fs::create_dir_all(&nas).unwrap();
        let nas_file = nas.join("DSC153.ARW");
        std::fs::write(&nas_file, b"raw").unwrap();
        std::fs::write(crate::xmp::sidecar_path(&nas_file), LIGHTROOM).unwrap();
        c(&state, |c| {
            let volume = c.add_volume("NAS", &nas, crate::catalog::VolumeKind::Backup).unwrap();
            c.add_location(id, volume, "DSC153.ARW", crate::catalog::LocationRole::Backup).unwrap();
        });

        let library = dir.join("library");
        let away = dir.join("library.away");
        std::fs::rename(&library, &away).unwrap();
        let nas_writer = WriteOrder::reserve(&nas_file);
        let state = std::sync::Arc::new(state);
        let retry = {
            let (state, uuid) = (state.clone(), row.uuid.clone());
            std::thread::spawn(move || retry_owed_iptc_as(&state, None, id, &uuid))
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!retry.is_finished(), "Retry must wait behind the NAS sidecar's writer");

        std::fs::rename(&away, &library).unwrap();
        let library_writer = WriteOrder::reserve(&file).wait();
        drop(nas_writer);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!retry.is_finished(), "Retry wrote the library sidecar under the NAS sidecar's turn");
        assert_eq!(std::fs::read_to_string(crate::xmp::sidecar_path(&file)).unwrap(), LIGHTROOM);
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::TITLE);

        drop(library_writer);
        let outcome = retry.join().unwrap().unwrap();
        assert_eq!(outcome.sidecar, IptcSidecarState::Written, "{outcome:?}");
        let xml = std::fs::read_to_string(crate::xmp::sidecar_path(&file)).unwrap();
        assert_eq!(iptc(&xml), with(foreign_iptc(), "dc:title", &["Mine"]), "{xml}");
        assert_eq!(std::fs::read_to_string(crate::xmp::sidecar_path(&nas_file)).unwrap(), LIGHTROOM);
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::NONE);
    }

    /// A photo id that no longer names the row's UUID: neither Retry nor Dismiss touches the
    /// photo that took it.
    #[test]
    fn retry_and_dismiss_refuse_an_id_that_names_another_photo_now() {
        let (_dir, state, id, _) = photo("iptc-153-retry-uuid");
        let row = owe(&state, id, &titled("Mine"));
        let other = format!("{}-not", row.uuid);
        assert_eq!(retry_owed_iptc_as(&state, None, id, &other).unwrap_err(), OWED_PHOTO_GONE);
        assert!(!dismiss_owed_iptc_as(&state, None, id, &other, row.generation).unwrap());
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::TITLE, "still owed, unwritten");
        assert!(dismiss_owed_iptc_as(&state, None, id, &row.uuid, row.generation).unwrap());
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::NONE);
    }

    /// Catalog B is open now, with the same photo id, UUID and generation owing IPTC. A
    /// Retry or Dismiss bound to A fails closed and leaves B's debt alone.
    #[test]
    fn a_bound_action_never_reaches_the_catalog_opened_since() {
        let (dir, state, id, _) = photo("iptc-153-switch");
        let row = owe(&state, id, &titled("A's"));
        let a = crate::app::catalog_identity(&state).unwrap();

        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let file = other.join("DSC153.ARW");
        std::fs::write(&file, b"raw").unwrap();
        let b = Catalog::open(&dir.join("b.chairphoto"), &other).unwrap();
        let b_id = b.upsert_photo(&file, None, 0, 1).unwrap().id;
        assert_eq!(b_id, id, "the ids collide");
        // A copied catalog: the same UUID too.
        b.conn().execute("UPDATE photos SET uuid = ?1 WHERE id = ?2", rusqlite::params![row.uuid, b_id]).unwrap();
        let w = b.set_iptc(b_id, &titled("B's")).unwrap();
        b.settle_iptc_write(&w, &Err("read-only".into())).unwrap();
        assert_eq!(b.list_owed_iptc_page(10, 0).unwrap()[0].generation, row.generation, "the generations collide");
        *state.catalog.lock().unwrap() = Some(b);

        let changed = crate::app::CATALOG_CHANGED;
        assert_eq!(retry_owed_iptc_as(&state, Some(a), id, &row.uuid).unwrap_err(), changed);
        assert_eq!(dismiss_owed_iptc_as(&state, Some(a), id, &row.uuid, row.generation).unwrap_err(), changed);
        assert_eq!(c(&state, |c| c.owed_iptc(id).unwrap()), IptcMask::TITLE, "B still owes");
        assert!(!std::fs::read_to_string(crate::xmp::sidecar_path(&file)).unwrap_or_default().contains("B's"));
    }
}
