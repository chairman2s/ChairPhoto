//! Authored IPTC the catalog holds that the photo's sidecar has not received yet (#148).
//!
//! An IPTC save stores the catalog row first and writes the sidecar after the lock is
//! released, touching only the managed fields it owes (#144: a field the catalog never
//! changed is left as another tool wrote it). A sidecar write that fails after the store
//! — read-only storage, an unparseable sidecar, a volume unmounting mid-save — therefore
//! leaves fields the catalog holds and nothing would ever write: a re-save of the same
//! values changes nothing, and a later save writes only its own change.
//!
//! So the debt is recorded, in `pending_sidecar_iptc`, as a set of fields
//! ([`IptcMask`]) per photo:
//!
//! 1. **Owe in the store.** [`Catalog::set_iptc`] ORs the fields whose value it changed
//!    into the photo's `owed` set and bumps its `generation`, in the transaction that stores
//!    the values. Every store owes; there is no way to change catalog IPTC without it.
//! 2. **Write owed ∪ changed.** The returned [`IptcSidecarWrite`] names every owed field
//!    (this change's and any earlier one's) with the catalog's values now, so a re-save or a
//!    retry writes what an earlier failure left out — a cleared field included, which the
//!    write removes.
//! 3. **Clear by compare-and-set.** [`Catalog::settle_iptc_write`] clears `owed` only while
//!    `generation` is still the one the write read. A newer store in the meantime keeps its
//!    fields owed. And because the older write's bytes may land on disk *after* the newer
//!    one's, a superseded successful write owes again each field it wrote with a value the
//!    catalog no longer holds: it cannot prove those stale values were overwritten, so the
//!    next write puts the catalog's values back. A field it wrote with the current value is
//!    right whenever it landed, and is not owed again.
//! 4. **Retry.** The identity repair pass ([`Catalog::run_identity_repair`]) drains the
//!    owed set after the identity queue, under the same job, abort flag and progress. An
//!    unreachable original stays owed: unmounted storage is a normal state.
//!
//! The record is per photo, not per copy: an IPTC save writes the sidecar beside the copy
//! the location resolver picks, and so does the retry.

use super::{Catalog, IptcFields, Result};
use rusqlite::{params, OptionalExtension};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// A set of the managed IPTC fields — the ones `xmp::write_iptc` writes.
///
/// The bit of each field is persisted in `pending_sidecar_iptc.owed`: never renumber one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct IptcMask(u16);

impl IptcMask {
    pub const NONE: Self = Self(0);
    pub const DESCRIPTION: Self = Self(1 << 0);
    pub const HEADLINE: Self = Self(1 << 1);
    pub const TITLE: Self = Self(1 << 2);
    pub const CREATOR: Self = Self(1 << 3);
    pub const COPYRIGHT: Self = Self(1 << 4);
    pub const CREDIT: Self = Self(1 << 5);
    pub const SOURCE: Self = Self(1 << 6);
    pub const CITY: Self = Self(1 << 7);
    pub const STATE: Self = Self(1 << 8);
    pub const COUNTRY: Self = Self(1 << 9);
    pub const COUNTRY_CODE: Self = Self(1 << 10);

    /// Every field, one mask each.
    pub const EACH: [Self; 11] = [
        Self::DESCRIPTION,
        Self::HEADLINE,
        Self::TITLE,
        Self::CREATOR,
        Self::COPYRIGHT,
        Self::CREDIT,
        Self::SOURCE,
        Self::CITY,
        Self::STATE,
        Self::COUNTRY,
        Self::COUNTRY_CODE,
    ];

    /// The value of the one field this mask names. Only meaningful for a single-field mask
    /// (one of [`Self::EACH`]); any other mask answers the empty string.
    pub fn value(self, f: &IptcFields) -> &str {
        match self {
            Self::DESCRIPTION => &f.description,
            Self::HEADLINE => &f.headline,
            Self::TITLE => &f.title,
            Self::CREATOR => &f.creator,
            Self::COPYRIGHT => &f.copyright,
            Self::CREDIT => &f.credit,
            Self::SOURCE => &f.source,
            Self::CITY => &f.city,
            Self::STATE => &f.state,
            Self::COUNTRY => &f.country,
            Self::COUNTRY_CODE => &f.country_code,
            _ => "",
        }
    }

    /// The fields whose value differs between `before` and `after`.
    pub fn changed(before: &IptcFields, after: &IptcFields) -> Self {
        Self::EACH
            .into_iter()
            .filter(|m| m.value(before) != m.value(after))
            .fold(Self::NONE, |a, m| a | m)
    }

    /// The fields in both sets.
    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The fields of this set that are not in `other`.
    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every field of `other` is in this set.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn bits(self) -> i64 {
        i64::from(self.0)
    }

    fn from_bits(bits: i64) -> Self {
        let all = Self::EACH.into_iter().fold(0u16, |a, m| a | m.0);
        Self((bits as u16) & all)
    }
}

impl std::ops::BitOr for IptcMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// One sidecar write of owed IPTC, read in the catalog transaction that stored the values
/// (or, for a retry, in one query): the fields to write, their values, and the version of
/// the owed record it answers for. Do the IO with [`run`](Self::run) off the catalog lock,
/// then record it with [`Catalog::settle_iptc_write`].
#[derive(Debug, Clone)]
pub struct IptcSidecarWrite {
    pub photo_id: i64,
    /// The photo's identity, so the compare-and-set cannot settle another photo that
    /// reused this id after a delete.
    uuid: String,
    /// The fields owed — this change's and any earlier write's that never landed.
    pub fields: IptcMask,
    /// The catalog's values now.
    pub values: IptcFields,
    /// The owed record's generation as read with `values`.
    generation: i64,
}

impl IptcSidecarWrite {
    /// Write the owed fields into the sidecar beside `original`. Pure filesystem work —
    /// call it off the catalog lock. Nothing owed opens nothing.
    ///
    /// The original was resolved under the lock, but its volume can go between then and
    /// now. A sidecar commit recreates missing parent directories, so without this check a
    /// write to an unmounted volume's mount point would land on the disk beneath it and be
    /// recorded as written. (A volume leaving between this check and the write is still
    /// possible; the window is the check's, not the save's.)
    pub fn run(&self, original: &Path) -> std::result::Result<(), String> {
        if self.fields.is_empty() {
            return Ok(());
        }
        if !original.exists() {
            return Err(format!("the original is no longer reachable at {}", original.display()));
        }
        crate::xmp::write_iptc_fields(original, self.fields, &self.values)
    }
}

/// What became of an owed IPTC write, as [`Catalog::settle_iptc_write`] recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IptcSettled {
    /// The owed fields are in the sidecar and nothing is owed any more.
    Written,
    /// Nothing was owed: the sidecar already had every value, and was not opened.
    Unchanged,
    /// The write landed, but a newer store changed the photo's IPTC meanwhile; its fields
    /// stay owed, and so do this write's whose value is no longer the catalog's (they may
    /// have landed after the newer write). When neither leaves anything owed, the settle
    /// is [`IptcSettled::Written`] instead.
    Superseded,
    /// The write failed — the reason is kept on the record — and the fields stay owed.
    Failed(String),
}

/// The sidecar half of an IPTC save, as a front end reports it. Serialized
/// `"written"` / `"unchanged"` / `"pending"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IptcSidecarState {
    /// The sidecar now has every IPTC value the catalog holds for the photo.
    Written,
    /// Nothing was owed, so nothing was written: the sidecar already had the values.
    Unchanged,
    /// Saved to the catalog only: some fields have not reached the sidecar yet. They stay
    /// owed, and the next save of the photo or the repair pass writes them.
    Pending,
}

impl IptcSettled {
    pub fn state(&self) -> IptcSidecarState {
        match self {
            IptcSettled::Written => IptcSidecarState::Written,
            IptcSettled::Unchanged => IptcSidecarState::Unchanged,
            IptcSettled::Superseded | IptcSettled::Failed(_) => IptcSidecarState::Pending,
        }
    }
}

/// How many photos the IPTC drain reads per query.
const IPTC_REPAIR_PAGE_SIZE: i64 = 256;

impl Catalog {
    /// Owe `changed` for `photo_id` (OR it into the set and bump the generation) and return
    /// the write that pays everything owed. PURE SQL; [`Catalog::set_iptc`] calls it inside
    /// the transaction that stores the values, which is what makes the generation and the
    /// values one snapshot.
    pub(super) fn owe_iptc(&self, photo_id: i64, changed: IptcMask) -> Result<IptcSidecarWrite> {
        if !changed.is_empty() {
            self.conn.execute(
                "INSERT INTO pending_sidecar_iptc (photo_id, owed, generation, queued_at)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(photo_id) DO UPDATE SET
                     queued_at = CASE WHEN owed = 0 THEN excluded.queued_at ELSE queued_at END,
                     owed = owed | excluded.owed,
                     generation = generation + 1",
                params![photo_id, changed.bits(), super::now()],
            )?;
        }
        Ok(self.owed_iptc_write(photo_id)?.unwrap_or_else(|| IptcSidecarWrite {
            photo_id,
            uuid: String::new(),
            fields: IptcMask::NONE,
            values: IptcFields::default(),
            generation: 0,
        }))
    }

    /// The write that pays `photo_id`'s owed IPTC now, or `None` when nothing is owed (or
    /// the photo is gone). One query, so the fields, values and generation agree.
    pub fn owed_iptc_write(&self, photo_id: i64) -> Result<Option<IptcSidecarWrite>> {
        Ok(self
            .conn
            .query_row(
                "SELECT q.owed, q.generation, p.uuid,
                        p.iptc_description, p.iptc_headline, p.iptc_title, p.iptc_creator,
                        p.iptc_copyright, p.iptc_credit, p.iptc_source, p.iptc_city,
                        p.iptc_state, p.iptc_country, p.iptc_country_code
                 FROM pending_sidecar_iptc q JOIN photos p ON p.id = q.photo_id
                 WHERE q.photo_id = ?1 AND q.owed != 0",
                params![photo_id],
                |r| {
                    Ok(IptcSidecarWrite {
                        photo_id,
                        fields: IptcMask::from_bits(r.get(0)?),
                        generation: r.get(1)?,
                        uuid: r.get(2)?,
                        values: IptcFields {
                            description: r.get(3)?,
                            headline: r.get(4)?,
                            title: r.get(5)?,
                            creator: r.get(6)?,
                            copyright: r.get(7)?,
                            credit: r.get(8)?,
                            source: r.get(9)?,
                            city: r.get(10)?,
                            state: r.get(11)?,
                            country: r.get(12)?,
                            country_code: r.get(13)?,
                        },
                    })
                },
            )
            .optional()?)
    }

    /// The fields `photo_id` owes its sidecar ([`IptcMask::NONE`] when none).
    pub fn owed_iptc(&self, photo_id: i64) -> Result<IptcMask> {
        Ok(self.owed_iptc_write(photo_id)?.map_or(IptcMask::NONE, |w| w.fields))
    }

    /// Photos owing IPTC to their sidecar.
    pub fn count_owed_iptc(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM pending_sidecar_iptc WHERE owed != 0", [], |r| r.get(0))?)
    }

    /// Record what became of `write`'s IO (`outcome`), by compare-and-set on the owed
    /// record's generation (this module's step 3). PURE SQL.
    pub fn settle_iptc_write(
        &self,
        write: &IptcSidecarWrite,
        outcome: &std::result::Result<(), String>,
    ) -> Result<IptcSettled> {
        if write.fields.is_empty() {
            return Ok(IptcSettled::Unchanged);
        }
        let now = super::now();
        match outcome {
            Ok(()) => {
                let cleared = self.conn.execute(
                    "UPDATE pending_sidecar_iptc
                     SET owed = 0, attempts = 0, error = '', last_attempt_at = ?3
                     WHERE photo_id = ?1 AND generation = ?2
                       AND EXISTS (SELECT 1 FROM photos WHERE id = ?1 AND uuid = ?4)",
                    params![write.photo_id, write.generation, now, write.uuid],
                )?;
                if cleared == 1 {
                    return Ok(IptcSettled::Written);
                }
                // A newer store ran meantime. Its own write may already have landed and
                // cleared the set, and this write's older values may have landed after it:
                // owe again each field this write put there with a value the catalog no
                // longer holds, so the next write restores it. A field it wrote with the
                // catalog's current value is right whenever its bytes landed (review of
                // #148, L6). A store after `current` is read owes what it changes itself,
                // and its write comes after this one's bytes, which have already landed.
                let current = match self.get_iptc(write.photo_id) {
                    Ok(current) => current,
                    Err(super::CatalogError::NotFound(_)) => return Ok(IptcSettled::Superseded),
                    Err(e) => return Err(e),
                };
                let stale = IptcMask::changed(&write.values, &current).intersect(write.fields);
                if !stale.is_empty() {
                    self.conn.execute(
                        "UPDATE pending_sidecar_iptc
                         SET owed = owed | ?2, generation = generation + 1
                         WHERE photo_id = ?1
                           AND EXISTS (SELECT 1 FROM photos WHERE id = ?1 AND uuid = ?3)",
                        params![write.photo_id, stale.bits(), write.uuid],
                    )?;
                    return Ok(IptcSettled::Superseded);
                }
                // Every value it wrote is the catalog's: if nothing else is owed (the newer
                // write has landed too), the sidecar has the catalog's IPTC.
                let still_owed: bool = self.conn.query_row(
                    "SELECT NOT EXISTS (SELECT 1 FROM photos WHERE id = ?1 AND uuid = ?2)
                         OR EXISTS (SELECT 1 FROM pending_sidecar_iptc WHERE photo_id = ?1 AND owed != 0)",
                    params![write.photo_id, write.uuid],
                    |r| r.get(0),
                )?;
                Ok(if still_owed { IptcSettled::Superseded } else { IptcSettled::Written })
            }
            Err(e) => {
                // Only this generation's record: a newer store's row describes its own write.
                // Nothing landed, so nothing to owe again.
                self.conn.execute(
                    "UPDATE pending_sidecar_iptc
                     SET attempts = attempts + 1, error = ?3, last_attempt_at = ?4
                     WHERE photo_id = ?1 AND generation = ?2 AND owed != 0",
                    params![write.photo_id, write.generation, e, now],
                )?;
                Ok(IptcSettled::Failed(e.clone()))
            }
        }
    }

    /// Resolve the original, write `photo_id`'s owed IPTC and settle it — the whole retry
    /// of one photo on this connection. Blocking (sidecar IO). `None` when nothing is owed.
    /// An unreachable original is [`IptcSettled::Failed`] and stays owed.
    pub fn write_owed_iptc(&self, photo_id: i64) -> Result<Option<(IptcSettled, bool)>> {
        let Some(write) = self.owed_iptc_write(photo_id)? else {
            return Ok(None);
        };
        let (outcome, reachable) = match self.resolve_photo_path(photo_id)? {
            Some(original) => (write.run(&original), true),
            None => (Err(format!("no reachable copy of photo {photo_id}")), false),
        };
        Ok(Some((self.settle_iptc_write(&write, &outcome)?, reachable)))
    }

    /// The IPTC half of the repair pass: retry every photo owing IPTC, a keyset page at a
    /// time, tallying into `summary`. Returns `false` when `abort` stopped it.
    pub(super) fn run_iptc_repair(
        &self,
        abort: &AtomicBool,
        summary: &mut super::IdentityRepairSummary,
        progress: &mut impl FnMut(&super::IdentityRepairSummary),
    ) -> Result<bool> {
        let mut after = 0i64;
        loop {
            let page: Vec<i64> = {
                let mut stmt = self.conn.prepare(
                    "SELECT photo_id FROM pending_sidecar_iptc
                     WHERE owed != 0 AND photo_id > ?1 ORDER BY photo_id LIMIT ?2",
                )?;
                let ids = stmt
                    .query_map(params![after, IPTC_REPAIR_PAGE_SIZE], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                ids
            };
            if page.is_empty() {
                return Ok(true);
            }
            for photo_id in page {
                if abort.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                after = photo_id;
                match self.write_owed_iptc(photo_id)? {
                    // Written by a save since the page was read.
                    None => summary.superseded += 1,
                    Some((IptcSettled::Written | IptcSettled::Unchanged, _)) => summary.iptc_written += 1,
                    Some((IptcSettled::Superseded, _)) => summary.superseded += 1,
                    Some((IptcSettled::Failed(_), false)) => summary.iptc_unreachable += 1,
                    Some((IptcSettled::Failed(_), true)) => summary.iptc_failed += 1,
                }
                progress(summary);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

    fn photo(tag: &str) -> (TestTmpDir, Catalog, i64, std::path::PathBuf) {
        let dir = TestTmpDir::new(tag);
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("DSC148.ARW");
        std::fs::write(&file, b"raw").unwrap();
        let catalog = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let id = catalog.upsert_photo(&file, None, 0, 1).unwrap().id;
        (dir, catalog, id, file)
    }

    fn titled(t: &str) -> IptcFields {
        IptcFields { title: t.into(), ..Default::default() }
    }

    /// Every field has its own bit, and `value` reads the field it names.
    #[test]
    fn each_mask_names_one_distinct_field() {
        let f = IptcFields {
            description: "description".into(),
            headline: "headline".into(),
            title: "title".into(),
            creator: "creator".into(),
            copyright: "copyright".into(),
            credit: "credit".into(),
            source: "source".into(),
            city: "city".into(),
            state: "state".into(),
            country: "country".into(),
            country_code: "country_code".into(),
        };
        let values: std::collections::HashSet<&str> = IptcMask::EACH.iter().map(|m| m.value(&f)).collect();
        assert_eq!(values.len(), 11);
        assert!(IptcMask::EACH.iter().all(|m| m.0.count_ones() == 1));
        assert_eq!(IptcMask::changed(&IptcFields::default(), &f).0.count_ones(), 11);
        assert_eq!(IptcMask::changed(&f, &f), IptcMask::NONE);
    }

    /// A store owes what it changed, in the same transaction; a no-change store owes nothing.
    #[test]
    fn a_store_owes_the_fields_it_changed() {
        let (_dir, c, id, _) = photo("iptc-owed-store");
        let w = c.set_iptc(id, &titled("A")).unwrap();
        assert_eq!(w.fields, IptcMask::TITLE);
        assert_eq!(w.values, titled("A"));
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::TITLE);
        assert_eq!(c.count_owed_iptc().unwrap(), 1);

        assert_eq!(c.settle_iptc_write(&w, &Ok(())).unwrap(), IptcSettled::Written);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::NONE);
        assert_eq!(c.count_owed_iptc().unwrap(), 0);

        let again = c.set_iptc(id, &titled("A")).unwrap();
        assert!(again.fields.is_empty(), "nothing changed and nothing owed");
        assert_eq!(c.settle_iptc_write(&again, &Ok(())).unwrap(), IptcSettled::Unchanged);
    }

    /// A failed write leaves the fields owed, and the next store's write carries them.
    #[test]
    fn a_failed_write_stays_owed_and_the_next_store_carries_it() {
        let (_dir, c, id, _) = photo("iptc-owed-fail");
        let w = c.set_iptc(id, &titled("A")).unwrap();
        let settled = c.settle_iptc_write(&w, &Err("read-only".into())).unwrap();
        assert_eq!(settled, IptcSettled::Failed("read-only".into()));
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::TITLE);

        let next = c.set_iptc(id, &IptcFields { headline: "H".into(), ..titled("A") }).unwrap();
        assert_eq!(next.fields, IptcMask::TITLE | IptcMask::HEADLINE);
    }

    /// The compare-and-set: a write read before a newer store cannot clear the newer
    /// store's fields, and re-owes its own (its bytes may land after the newer write).
    #[test]
    fn a_stale_write_does_not_clear_a_newer_stores_debt() {
        let (_dir, c, id, _) = photo("iptc-owed-cas");
        let older = c.set_iptc(id, &titled("A")).unwrap();
        let newer = c.set_iptc(id, &IptcFields { creator: "C".into(), ..titled("B") }).unwrap();

        assert_eq!(c.settle_iptc_write(&older, &Ok(())).unwrap(), IptcSettled::Superseded);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::TITLE | IptcMask::CREATOR);

        // The newer write was read before the stale one re-owed, so it is superseded too and
        // the debt survives for the next write — which carries the catalog's values.
        assert_eq!(c.settle_iptc_write(&newer, &Ok(())).unwrap(), IptcSettled::Superseded);
        let retry = c.owed_iptc_write(id).unwrap().unwrap();
        assert_eq!(retry.values.title, "B");
        assert_eq!(c.settle_iptc_write(&retry, &Ok(())).unwrap(), IptcSettled::Written);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::NONE);
    }

    /// The newer write settling first and the stale one after: the stale bytes may be the
    /// last on disk, so its fields are owed again rather than left cleared.
    #[test]
    fn a_stale_write_settling_after_the_newer_one_owes_its_fields_again() {
        let (_dir, c, id, _) = photo("iptc-owed-late");
        let older = c.set_iptc(id, &titled("A")).unwrap();
        let newer = c.set_iptc(id, &titled("B")).unwrap();
        assert_eq!(c.settle_iptc_write(&newer, &Ok(())).unwrap(), IptcSettled::Written);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::NONE);
        assert_eq!(c.settle_iptc_write(&older, &Ok(())).unwrap(), IptcSettled::Superseded);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::TITLE);
    }

    /// Review of #148, L2 (its probe P3): the photo a write was read for is removed and its
    /// id reused by a new photo, whose own store lands on the same generation. The stale
    /// write's settle matches on id and generation, so only the UUID tells the photos apart:
    /// it must not clear the new photo's debt.
    #[test]
    fn a_stale_write_for_a_removed_photo_does_not_settle_the_photo_that_reused_its_id() {
        let (dir, c, id, _) = photo("iptc-owed-reused-id");
        let stale = c.set_iptc(id, &titled("old photo")).unwrap();
        c.remove_photo(id).unwrap();
        let file = dir.join("library").join("OTHER.ARW");
        std::fs::write(&file, b"raw").unwrap();
        let reused = c.upsert_photo(&file, None, 0, 1).unwrap().id;
        assert_eq!(reused, id, "the precondition: the id is reused");
        let fresh = c.set_iptc(reused, &titled("new photo")).unwrap();
        assert_eq!(fresh.generation, stale.generation, "the precondition: the generations collide");

        assert_eq!(c.settle_iptc_write(&stale, &Ok(())).unwrap(), IptcSettled::Superseded);
        assert_eq!(c.owed_iptc(reused).unwrap(), IptcMask::TITLE, "the new photo still owes its title");
        assert_eq!(c.owed_iptc_write(reused).unwrap().unwrap().values.title, "new photo");
    }

    /// Review of #148, L6: a superseded write that landed re-owes only the fields it wrote
    /// with a value the catalog no longer holds. Here the newer store changed the title
    /// but kept the headline the older write also carried: only the title is owed again.
    #[test]
    fn a_superseded_write_re_owes_only_the_fields_whose_value_changed() {
        let (_dir, c, id, _) = photo("iptc-owed-reowe-changed");
        let older = c.set_iptc(id, &IptcFields { headline: "H".into(), ..titled("A") }).unwrap();
        assert_eq!(older.fields, IptcMask::TITLE | IptcMask::HEADLINE);
        let newer = c.set_iptc(id, &IptcFields { headline: "H".into(), ..titled("B") }).unwrap();
        assert_eq!(c.settle_iptc_write(&newer, &Ok(())).unwrap(), IptcSettled::Written);

        // The older write's bytes land last.
        assert_eq!(c.settle_iptc_write(&older, &Ok(())).unwrap(), IptcSettled::Superseded);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::TITLE, "the headline it wrote is still the catalog's");
    }

    /// The other case: every value the superseded write put in the sidecar is still the
    /// catalog's (the newer store changed only a field it did not write), so nothing is owed
    /// again, and with the newer write landed the save reports written, not pending.
    #[test]
    fn a_superseded_write_whose_values_are_current_owes_nothing() {
        let (_dir, c, id, _) = photo("iptc-owed-reowe-none");
        let older = c.set_iptc(id, &titled("A")).unwrap();
        let newer = c.set_iptc(id, &IptcFields { creator: "C".into(), ..titled("A") }).unwrap();
        assert_eq!(newer.fields, IptcMask::TITLE | IptcMask::CREATOR);
        assert_eq!(c.settle_iptc_write(&newer, &Ok(())).unwrap(), IptcSettled::Written);

        assert_eq!(c.settle_iptc_write(&older, &Ok(())).unwrap(), IptcSettled::Written);
        assert_eq!(c.owed_iptc(id).unwrap(), IptcMask::NONE);
    }

    /// The repair pass drains owed IPTC under its abort flag: a cancel between photos stops
    /// it with the rest still owed, and the summary says it stopped.
    #[test]
    fn the_pass_stops_at_its_abort_flag_between_owed_photos() {
        let (dir, c, first, _) = photo("iptc-owed-abort");
        let second_file = dir.join("library").join("DSC149.ARW");
        std::fs::write(&second_file, b"raw").unwrap();
        let second = c.upsert_photo(&second_file, None, 0, 1).unwrap().id;
        c.set_iptc(first, &titled("A")).unwrap();
        c.set_iptc(second, &titled("B")).unwrap();
        // Only the IPTC drain is under test: clear the identity rows the upserts queued.
        c.conn.execute("DELETE FROM pending_sidecar_identity", []).unwrap();

        let abort = AtomicBool::new(false);
        let summary = c
            .run_identity_repair(&abort, |s| {
                if s.iptc_written == 1 {
                    abort.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert!(summary.aborted, "{summary:?}");
        assert_eq!((summary.iptc_written, summary.total, summary.done()), (1, 2, 1));
        assert_eq!(c.count_owed_iptc().unwrap(), 1);

        let summary = c.repair_pending_identity().unwrap();
        assert_eq!((summary.iptc_written, summary.aborted), (1, false));
        assert_eq!(c.count_owed_iptc().unwrap(), 0);
    }

    /// A stale write's failure does not touch a newer store's record.
    #[test]
    fn a_stale_failure_leaves_the_newer_record_alone() {
        let (_dir, c, id, _) = photo("iptc-owed-stale-fail");
        let older = c.set_iptc(id, &titled("A")).unwrap();
        let _newer = c.set_iptc(id, &titled("B")).unwrap();
        c.settle_iptc_write(&older, &Err("gone".into())).unwrap();
        let (attempts, error): (i64, String) = c
            .conn
            .query_row("SELECT attempts, error FROM pending_sidecar_iptc WHERE photo_id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((attempts, error.as_str()), (0, ""));
    }
}
