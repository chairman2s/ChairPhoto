//! Trash: hiding a photo reversibly, without touching a byte (cluster B, B2).
//!
//! ## What trash is not
//!
//! Not `pick_state = 'reject'`. Reject is a culling verdict on a photo that stays in the
//! library and keeps showing up; trash hides the photo everywhere. Two-pass culling uses
//! reject, deletion flows use trash, and conflating them would make one of the two verbs
//! useless.
//!
//! Not a sidecar field either. No user culling state reaches an in-library sidecar today —
//! keywords go only to export destinations, and rating and colour label have no writer at
//! all — so putting trash there would make it the first, and would mark the file as
//! trashed for every foreign tool that reads that sidecar on the strength of a reversible
//! local decision.
//!
//! **Catalog-local**, like every other mutable per-photo state (`CONTEXT.md`). Merge is
//! additive-only, so trashing a photo on one device tells no other device anything. That is
//! a real limit, deliberately chosen over breaching additive-only, and it is recorded in
//! the vocabulary rather than hidden.
//!
//! ## Hiding is one predicate, in one place
//!
//! `trashed_at IS NULL` joins `photos_visible` and nothing else changes. Every query that
//! lists or counts photos for the user already reads that view (step 2), so trash reaches
//! all of them at once instead of being spelled out at 36 call sites.
//!
//! ## Why the timestamp is the group key
//!
//! `trashed_at` is a nullable timestamp rather than a boolean, which buys three things at
//! once: the trash view can order by it, "empty trash older than N days" is expressible,
//! and a stack trashed together can be restored together.
//!
//! That last one matters more than it looks. Stack children are hidden from the grid by
//! `stack_parent_id IS NULL`, so trashing a master would hide it by the trash predicate
//! while its children stayed hidden by the stack predicate — the trash view would list one
//! photo and the rest would be unreachable from every surface, since their only route into
//! the UI was the master's Stack section. Auto-stack proposals turn whole bursts into
//! stacks deliberately, so this is the normal case, not an edge one.
//!
//! So trashing a master cascades to its children with the *same* timestamp, and restoring
//! it restores exactly the frames carrying that timestamp. A child trashed on its own
//! earlier carries a different one and stays trashed — which is what makes restore exact
//! without a second column or a join table.
//!
//! The one seam: `trashed_at` is whole seconds, so a master and an unrelated child of that
//! same master trashed in the *same second* would restore together. That needs two
//! deliberate actions inside one second on one stack; the alternative is a group id whose
//! only job is to close that window.

use super::{Catalog, Photo, Result};
use rusqlite::params;
use serde::Serialize;

/// What one trash call did.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashSummary {
    /// Photos the caller named that were not already in the trash.
    pub trashed: usize,
    /// Stack children hidden along with a master the caller named.
    pub cascaded: usize,
    /// Photos the caller named that were already in the trash.
    pub already: usize,
}

impl Catalog {
    /// Move photos to the trash, taking each one's stack with it.
    ///
    /// Idempotent: a photo already in the trash keeps its original timestamp, so re-trashing
    /// a stack cannot silently re-group it with something else.
    pub fn trash_photos(&self, photo_ids: &[i64]) -> Result<TrashSummary> {
        let mut summary = TrashSummary::default();
        if photo_ids.is_empty() {
            return Ok(summary);
        }
        let at = now();
        let tx = self.conn.unchecked_transaction()?;
        for &id in photo_ids {
            // `Option<i64>` at the row level, not just at the statement level: a photo
            // that is not in the trash has a NULL here, and reading NULL as `i64` is an
            // error rather than an absence.
            let already = self
                .conn
                .query_row(
                    "-- includes-hidden: reads the trash state of one photo by id, which is only
                     -- answerable from the base table — the view exists to exclude
                     -- exactly these rows.
                     SELECT trashed_at FROM photos WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional_row()?
                .flatten();
            if already.is_some() {
                summary.already += 1;
                continue;
            }
            self.conn.execute(
                "UPDATE photos SET trashed_at = ?1, updated_at = ?2 WHERE id = ?3",
                params![at, at, id],
            )?;
            summary.trashed += 1;
            // Children that are already in the trash keep the timestamp they went in with:
            // they were a separate decision and must survive restoring this master.
            summary.cascaded += self.conn.execute(
                "UPDATE photos SET trashed_at = ?1, updated_at = ?2
                 WHERE stack_parent_id = ?3 AND trashed_at IS NULL",
                params![at, at, id],
            )?;
        }
        tx.commit()?;
        Ok(summary)
    }

    /// Bring photos back, along with whatever was trashed in the same act.
    ///
    /// Restoring a stack child does not restore its master: the child is what the user
    /// asked for, and un-hiding the master would be a decision they did not make.
    pub fn restore_photos(&self, photo_ids: &[i64]) -> Result<usize> {
        if photo_ids.is_empty() {
            return Ok(0);
        }
        let mut restored = 0usize;
        let tx = self.conn.unchecked_transaction()?;
        for &id in photo_ids {
            let Some(at) = self
                .conn
                .query_row(
                    "-- includes-hidden: reads the trash state of one photo by id, which is only
                     -- answerable from the base table — the view exists to exclude
                     -- exactly these rows.
                     SELECT trashed_at FROM photos WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional_row()?
                .flatten()
            else {
                continue; // not in the trash — nothing to undo
            };
            restored += self.conn.execute(
                "UPDATE photos SET trashed_at = NULL, updated_at = ?1 WHERE id = ?2",
                params![now(), id],
            )?;
            restored += self.conn.execute(
                "UPDATE photos SET trashed_at = NULL, updated_at = ?1
                 WHERE stack_parent_id = ?2 AND trashed_at = ?3",
                params![now(), id, at],
            )?;
        }
        tx.commit()?;
        Ok(restored)
    }

    /// Everything in the trash, most recently trashed first.
    ///
    /// Reads `photos` rather than `photos_visible` for the obvious reason (includes-hidden:
    /// this *is* the view of hidden photos), and lists stack children too — they are how a
    /// user sees what a cascade took with it.
    pub fn list_trash(&self) -> Result<Vec<Photo>> {
        let mut stmt = self.conn.prepare(&format!(
            "-- includes-hidden: the trash is the one surface whose whole job is to show
             -- photos the rest of the app hides.
             SELECT {cols} FROM photos
             WHERE trashed_at IS NOT NULL
             ORDER BY trashed_at DESC, id",
            cols = super::query::photo_columns("photos"),
        ))?;
        let rows = stmt.query_map([], super::row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Photos in the trash since before `cutoff` (unix seconds), oldest first — the
    /// candidates for "empty trash older than N days".
    pub fn trashed_before(&self, cutoff: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "-- includes-hidden: selects from the trash by definition.
             SELECT id FROM photos WHERE trashed_at IS NOT NULL AND trashed_at < ?1
             ORDER BY trashed_at, id",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Is this photo in the trash?
    pub fn is_trashed(&self, photo_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "-- includes-hidden: answers a question *about* hiddenness.
                 SELECT trashed_at IS NOT NULL FROM photos WHERE id = ?1",
                params![photo_id],
                |r| r.get::<_, bool>(0),
            )
            .optional_row()?
            .unwrap_or(false))
    }
}

/// `query_row` that maps "no such row" to `None` rather than an error.
trait OptionalRow<T> {
    fn optional_row(self) -> Result<Option<T>>;
}

impl<T> OptionalRow<T> for rusqlite::Result<T> {
    fn optional_row(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::PhotoQuery;

    fn catalog() -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new("trash");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    fn photo(c: &Catalog, root: &std::path::Path, name: &str) -> i64 {
        std::fs::write(root.join(name), b"x").unwrap();
        c.upsert_photo(&root.join(name), None, 1, 1).unwrap().id
    }

    /// Put a photo in the trash at a specific moment, so "trashed earlier, separately" is
    /// expressible without depending on whole-second wall-clock timing.
    fn trashed_at(c: &Catalog, id: i64, at: i64) {
        c.conn
            .execute("UPDATE photos SET trashed_at = ?1 WHERE id = ?2", params![at, id])
            .unwrap();
    }

    /// The risky part of trash is not the column, it is that every query has to learn about
    /// it. One assertion per surface, deliberately not one aggregate — an aggregate would
    /// pass while three of the five surfaces still leaked.
    #[test]
    fn a_trashed_photo_disappears_from_every_surface_that_counts_photos() {
        let (c, root) = catalog();
        let tag = c.create_tag("Trip/Oslo").unwrap();
        let keep = photo(&c, &root, "keep.arw");
        let gone = photo(&c, &root, "gone.arw");
        for id in [keep, gone] {
            c.assign_tag(id, tag).unwrap();
        }
        let album = c.create_album("A").unwrap();
        c.add_photos_to_album(album, &[keep, gone]).unwrap();

        let counts = |c: &Catalog| {
            (
                c.list_photos(&PhotoQuery::default()).unwrap().len(),
                c.list_albums().unwrap().iter().find(|a| a.id == album).unwrap().photo_count,
                c.catalog_stats(None, None, None).unwrap().total_photos,
                c.list_tags_with_counts()
                    .unwrap()
                    .iter()
                    .find(|t| t.tag.id == tag)
                    .map(|t| t.photo_count),
                c.library_safety_summary().unwrap().at_risk,
            )
        };
        let before = counts(&c);
        assert_eq!(before.0, 2, "both photos are listed to begin with");

        c.trash_photos(&[gone]).unwrap();
        let after = counts(&c);

        assert_eq!(after.0, 1, "the grid query");
        assert_eq!(after.1, before.1 - 1, "the album count");
        assert_eq!(after.2, before.2 - 1, "library stats");
        assert_eq!(after.3, before.3.map(|n| n - 1), "the tag count");
        assert_eq!(after.4, before.4 - 1, "the safety summary");

        c.restore_photos(&[gone]).unwrap();
        assert_eq!(counts(&c), before, "restore returns every count to where it was");
    }

    /// Trashing a stack master must not strand its frames. They are hidden from the grid by
    /// `stack_parent_id IS NULL`, so without the cascade the trash would list one photo and
    /// the rest would be unreachable from every surface at once.
    #[test]
    fn trashing_a_stack_master_takes_its_frames_and_gives_them_back() {
        let (c, root) = catalog();
        let master = photo(&c, &root, "m.arw");
        let a = photo(&c, &root, "a.arw");
        let b = photo(&c, &root, "b.arw");
        c.set_stack_parent(a, master).unwrap();
        c.set_stack_parent(b, master).unwrap();

        let s = c.trash_photos(&[master]).unwrap();

        assert_eq!((s.trashed, s.cascaded), (1, 2), "the master, and both frames with it");
        for id in [master, a, b] {
            assert!(c.is_trashed(id).unwrap(), "photo {id} is hidden");
        }
        assert_eq!(c.list_trash().unwrap().len(), 3, "and all three are findable there");

        c.restore_photos(&[master]).unwrap();
        for id in [master, a, b] {
            assert!(!c.is_trashed(id).unwrap(), "photo {id} came back");
        }
    }

    /// A frame trashed on its own was a separate decision. Restoring the master it happens
    /// to sit under must not undo it — that is what keying the cascade on the timestamp
    /// buys, and it is the assertion that makes "restore is exact" mean something.
    #[test]
    fn a_frame_trashed_separately_survives_restoring_its_master() {
        let (c, root) = catalog();
        let master = photo(&c, &root, "m.arw");
        let with_master = photo(&c, &root, "a.arw");
        let on_its_own = photo(&c, &root, "b.arw");
        c.set_stack_parent(with_master, master).unwrap();
        c.set_stack_parent(on_its_own, master).unwrap();
        trashed_at(&c, on_its_own, 1000);

        let s = c.trash_photos(&[master]).unwrap();
        assert_eq!(s.cascaded, 1, "only the untrashed frame joins the group");

        c.restore_photos(&[master]).unwrap();

        assert!(!c.is_trashed(master).unwrap());
        assert!(!c.is_trashed(with_master).unwrap(), "the frame trashed with it returns");
        assert!(
            c.is_trashed(on_its_own).unwrap(),
            "the frame trashed separately stays where the user put it"
        );
    }

    /// Restoring a frame is not a decision about its master.
    #[test]
    fn restoring_a_frame_leaves_its_master_in_the_trash() {
        let (c, root) = catalog();
        let master = photo(&c, &root, "m.arw");
        let frame = photo(&c, &root, "a.arw");
        c.set_stack_parent(frame, master).unwrap();
        c.trash_photos(&[master]).unwrap();

        c.restore_photos(&[frame]).unwrap();

        assert!(!c.is_trashed(frame).unwrap());
        assert!(c.is_trashed(master).unwrap(), "the master was not what the user asked for");
    }

    /// Trashing a frame drops the master's stack badge, because that is a decision about
    /// this photo — unlike an offline child, which still counts.
    #[test]
    fn a_trashed_frame_stops_counting_toward_the_stack_badge() {
        let (c, root) = catalog();
        let master = photo(&c, &root, "m.arw");
        let a = photo(&c, &root, "a.arw");
        let b = photo(&c, &root, "b.arw");
        c.set_stack_parent(a, master).unwrap();
        c.set_stack_parent(b, master).unwrap();

        let badge = |c: &Catalog| c.get_photo(master).unwrap().stack_count;
        assert_eq!(badge(&c), 2);

        c.trash_photos(&[a]).unwrap();
        assert_eq!(badge(&c), 1, "the trashed frame is no longer claimed");

        c.restore_photos(&[a]).unwrap();
        assert_eq!(badge(&c), 2);
    }

    /// Re-trashing must not re-group: a photo already in the trash would otherwise silently
    /// join whatever stack was trashed most recently, and come back with it.
    #[test]
    fn trashing_something_already_in_the_trash_leaves_its_group_alone() {
        let (c, root) = catalog();
        let id = photo(&c, &root, "a.arw");
        trashed_at(&c, id, 500);

        let s = c.trash_photos(&[id]).unwrap();

        assert_eq!((s.trashed, s.already), (0, 1));
        let at: i64 = c
            .conn
            .query_row("-- includes-hidden: reads the trash state of one photo by id, which is only
                     -- answerable from the base table — the view exists to exclude
                     -- exactly these rows.
                     SELECT trashed_at FROM photos WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(at, 500, "the original timestamp — and so the original group — is kept");
    }

    #[test]
    fn the_trash_lists_newest_first_and_selects_by_age() {
        let (c, root) = catalog();
        let old = photo(&c, &root, "old.arw");
        let new = photo(&c, &root, "new.arw");
        trashed_at(&c, old, 1_000);
        trashed_at(&c, new, 2_000);

        let listed: Vec<i64> = c.list_trash().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(listed, vec![new, old], "most recently trashed first");
        assert_eq!(c.trashed_before(1_500).unwrap(), vec![old], "and age selects the old one");
    }
}
