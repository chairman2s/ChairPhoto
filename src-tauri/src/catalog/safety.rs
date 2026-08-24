//! Would I lose this photo if a disk died? (cluster B, B1.)
//!
//! This is a **second axis**, deliberately not folded into [`StorageStatus`]. They answer
//! different questions and are consumed in different places:
//!
//! - `StorageStatus` — *can I display this photo right now*. Drives the grid badge, so it
//!   is on the hot path and must not care about hash verification.
//! - `SafetyStatus` — *would I lose it*. Drives one panel, consulted deliberately.
//!
//! ## What "at risk" means here, and why it is not "one disk"
//!
//! `CONTEXT.md` carries two definitions that are not complements: *at risk* is "exists on
//! exactly one disk", while *safe* is "home holds a verified copy". Home holding the **only**
//! copy satisfies the second and violates the first. On the owner's library the two differ by
//! 164,185 photos.
//!
//! The predicate here is home-possession, not copy count, and that is a deliberate choice
//! about facts the catalog cannot see: home is a RAID array that is itself backed up
//! off-site, so counting volumes would raise an alarm on 99.5% of the library for photos
//! that are in no danger — and an alarm that is wrong the first time is one nobody reads
//! again. The cost of the choice is that the panel must say what it *cannot* see, because
//! the off-site copy is invisible until that integration exists.
//!
//! ## The buckets
//!
//! Ordered from worst to best; a photo lands in the first one it matches.
//!
//! | | meaning |
//! |---|---|
//! | `Missing` | no copy recorded anywhere |
//! | `AtRisk` | no copy at home |
//! | `Unverified` | a copy at home that has never been hash-verified |
//! | `Stale` | verified at home, but a companion has moved on locally |
//! | `Safe` | verified at home, companions carried and current |
//!
//! `Stale` exists because a copy is the image *plus* its companions (D2): a photo whose
//! pixels are safe but whose develop history only exists on one disk is not safe, and
//! saying so is the whole point of the distinction.
//!
//! ## Two shapes of one rule
//!
//! The summary counts with a single grouped pass (0.24 s over 165k photos on the owner's
//! catalog, versus 0.56 s for the correlated form), while the grid filter needs a per-row
//! `EXISTS`. That is two spellings of one meaning, which is exactly how rules drift apart —
//! so [`HOME_COPY_EXISTS`] is shared where it can be, and
//! `the_two_spellings_of_home_agree` pins the rest.
//!
//! **No filesystem access.** Every query here is pure SQL, so an unmounted NAS cannot make
//! the panel hang. That is why freshness is *recorded* by the scanner rather than computed
//! on read (D5), and why the figure is only ever true as of the last scan.

use super::{Catalog, Result};
use serde::Serialize;

/// A copy that counts as *home*: on a backup-kind volume, in a role that means safety.
///
/// An `export` copy is a one-way hand-off and never counts, even when the user pointed the
/// export at a backup disk — the same rule `status_from_locations` applies.
///
/// Written against a `photos` row aliased `p`, for splicing into the grid query.
pub(crate) const HOME_COPY_EXISTS: &str = "EXISTS (SELECT 1 FROM photo_locations l \
     JOIN volumes v ON v.id = l.volume_id \
     WHERE l.photo_id = p.id AND v.kind = 'backup' AND l.role IN ('primary','backup'))";

/// Any copy at all, excluding one-way exports.
pub(crate) const ANY_COPY_EXISTS: &str =
    "EXISTS (SELECT 1 FROM photo_locations l WHERE l.photo_id = p.id AND l.role <> 'export')";

/// Where a photo sits on the "would I lose it" axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SafetyStatus {
    /// No copy recorded anywhere.
    Missing,
    /// No copy at home.
    AtRisk,
    /// A copy at home, never hash-verified.
    Unverified,
    /// Verified at home, but a companion has moved on locally since it was carried.
    Stale,
    /// Verified at home, companions carried and current.
    Safe,
}

/// Library-wide counts for the safety panel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetySummary {
    pub missing: i64,
    pub at_risk: i64,
    pub unverified: i64,
    pub stale: i64,
    pub safe: i64,
    /// `created_at` of the oldest at-risk photo — how long the library has been in this
    /// state, which reads very differently from the count alone.
    pub oldest_at_risk: Option<i64>,
    /// Carried companions the scanner has looked at since they were carried. Freshness is
    /// only known for these.
    pub companions_checked: i64,
    /// Carried companions not looked at since. The `stale` count is a floor, not a total,
    /// while this is non-zero — and the panel has to say so.
    pub companions_unchecked: i64,
}

impl Catalog {
    /// Count the library by safety bucket. Pure SQL: no volume is stat-ed, so an unmounted
    /// NAS cannot slow this down or make it fail.
    ///
    /// One grouped pass over `photo_locations` rather than four correlated sub-selects per
    /// photo — measured at 0.24 s versus 0.56 s over 165,093 photos, same answers.
    pub fn library_safety_summary(&self) -> Result<SafetySummary> {
        let sql = "
            WITH loc AS (
                SELECT l.photo_id AS pid,
                       MAX(l.role <> 'export') AS any_copy,
                       MAX(v.kind = 'backup' AND l.role IN ('primary','backup')) AS home,
                       MAX(v.kind = 'backup' AND l.role IN ('primary','backup')
                           AND l.verified_hash IS NOT NULL) AS verified
                FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                GROUP BY l.photo_id
            ),
            stale AS (
                SELECT DISTINCT l.photo_id AS pid
                FROM photo_location_companions c
                JOIN photo_locations l ON l.id = c.location_id
                WHERE c.source_mtime_seen IS NOT NULL
                  AND c.source_mtime_seen > c.carried_mtime
            )
            SELECT
                SUM(CASE WHEN COALESCE(any_copy, 0) = 0 THEN 1 ELSE 0 END),
                SUM(CASE WHEN COALESCE(any_copy, 0) = 1
                          AND COALESCE(home, 0) = 0 THEN 1 ELSE 0 END),
                SUM(CASE WHEN COALESCE(home, 0) = 1
                          AND COALESCE(verified, 0) = 0 THEN 1 ELSE 0 END),
                SUM(CASE WHEN COALESCE(home, 0) = 1 AND COALESCE(verified, 0) = 1
                          AND s.pid IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN COALESCE(home, 0) = 1 AND COALESCE(verified, 0) = 1
                          AND s.pid IS NULL THEN 1 ELSE 0 END),
                MIN(CASE WHEN COALESCE(any_copy, 0) = 1
                          AND COALESCE(home, 0) = 0 THEN p.created_at END)
            FROM photos_visible p
            LEFT JOIN loc ON loc.pid = p.id
            LEFT JOIN stale s ON s.pid = p.id";

        let (missing, at_risk, unverified, stale, safe, oldest_at_risk) =
            self.conn.query_row(sql, [], |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?,
                ))
            })?;

        let (companions_checked, companions_unchecked) = self.conn.query_row(
            "SELECT SUM(source_mtime_seen IS NOT NULL), SUM(source_mtime_seen IS NULL)
             FROM photo_location_companions",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                ))
            },
        )?;

        Ok(SafetySummary {
            missing,
            at_risk,
            unverified,
            stale,
            safe,
            oldest_at_risk,
            companions_checked,
            companions_unchecked,
        })
    }

    /// One photo's safety bucket, for the inspector.
    pub fn photo_safety_status(&self, photo_id: i64) -> Result<SafetyStatus> {
        let (any_copy, home, verified, stale) = self.conn.query_row(
            "SELECT
                 EXISTS (SELECT 1 FROM photo_locations l
                         WHERE l.photo_id = ?1 AND l.role <> 'export'),
                 EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                         WHERE l.photo_id = ?1 AND v.kind = 'backup'
                           AND l.role IN ('primary','backup')),
                 EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                         WHERE l.photo_id = ?1 AND v.kind = 'backup'
                           AND l.role IN ('primary','backup') AND l.verified_hash IS NOT NULL),
                 EXISTS (SELECT 1 FROM photo_location_companions c
                         JOIN photo_locations l ON l.id = c.location_id
                         WHERE l.photo_id = ?1 AND c.source_mtime_seen IS NOT NULL
                           AND c.source_mtime_seen > c.carried_mtime)",
            [photo_id],
            |r| {
                Ok((
                    r.get::<_, bool>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            },
        )?;
        Ok(match (any_copy, home, verified, stale) {
            (false, _, _, _) => SafetyStatus::Missing,
            (true, false, _, _) => SafetyStatus::AtRisk,
            (true, true, false, _) => SafetyStatus::Unverified,
            (true, true, true, true) => SafetyStatus::Stale,
            (true, true, true, false) => SafetyStatus::Safe,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{LocationRole, VolumeKind};

    fn catalog() -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new("safety");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    /// Build one photo of every safety shape. Returns (nas volume id, ids by bucket).
    fn library(c: &Catalog, root: &std::path::Path) -> (i64, Vec<(SafetyStatus, i64)>) {
        let nas_dir = root.parent().unwrap().join("nas");
        std::fs::create_dir_all(&nas_dir).unwrap();
        let nas = c.add_volume("NAS", &nas_dir, VolumeKind::Backup).unwrap();
        let local = c
            .list_volumes()
            .unwrap()
            .into_iter()
            .find(|v| v.kind == VolumeKind::Local)
            .unwrap()
            .id;

        let mk = |name: &str| {
            std::fs::write(root.join(name), b"x").unwrap();
            c.upsert_photo(&root.join(name), None, 1, 1).unwrap().id
        };

        // AtRisk: local only.
        let at_risk = mk("at-risk.arw");

        // Missing: no location rows at all.
        let missing = mk("missing.arw");
        c.remove_locations_on_volume(missing, local).unwrap();

        // Unverified: a home copy with no verified hash.
        let unverified = mk("unverified.arw");
        c.add_location(unverified, nas, "unverified.arw", LocationRole::Backup).unwrap();

        // Safe: a verified home copy.
        let safe = mk("safe.arw");
        std::fs::write(nas_dir.join("safe.arw"), b"x").unwrap();
        c.backup_photo(safe, nas).unwrap();

        // Stale: verified home copy whose companion has moved on locally.
        let stale = mk("stale.arw");
        std::fs::write(root.join("stale.arw.rrdata"), b"edit").unwrap();
        c.backup_photo(stale, nas).unwrap();
        c.conn()
            .execute(
                "UPDATE photo_location_companions SET source_mtime_seen = carried_mtime + 60",
                [],
            )
            .unwrap();

        // An export copy on the NAS must not rescue an at-risk photo.
        let exported = mk("exported.arw");
        c.add_location(exported, nas, "exported.arw", LocationRole::Export).unwrap();

        (
            nas,
            vec![
                (SafetyStatus::AtRisk, at_risk),
                (SafetyStatus::Missing, missing),
                (SafetyStatus::Unverified, unverified),
                (SafetyStatus::Safe, safe),
                (SafetyStatus::Stale, stale),
                (SafetyStatus::AtRisk, exported),
            ],
        )
    }

    #[test]
    fn every_photo_lands_in_the_bucket_its_copies_imply() {
        let (c, root) = catalog();
        let (_nas, photos) = library(&c, &root);

        for (expected, id) in photos {
            assert_eq!(c.photo_safety_status(id).unwrap(), expected, "photo {id}");
        }
    }

    #[test]
    fn the_summary_counts_the_same_buckets_the_per_photo_status_reports() {
        // Two spellings of one rule — a grouped pass and a per-row EXISTS. This is the
        // assertion that stops them drifting.
        let (c, root) = catalog();
        let (_nas, photos) = library(&c, &root);
        let s = c.library_safety_summary().unwrap();

        let count = |want: SafetyStatus| {
            photos.iter().filter(|(got, _)| *got == want).count() as i64
        };
        assert_eq!(s.at_risk, count(SafetyStatus::AtRisk), "at risk");
        assert_eq!(s.unverified, count(SafetyStatus::Unverified), "unverified");
        assert_eq!(s.stale, count(SafetyStatus::Stale), "stale");
        assert_eq!(s.safe, count(SafetyStatus::Safe), "safe");
        assert_eq!(s.missing, count(SafetyStatus::Missing), "missing");
    }

    /// The summary counts with a grouped pass; the grid filters with a per-row `EXISTS`.
    /// If those two spellings ever disagree, the panel says "13 at risk" and then shows
    /// you a different set — which is worse than either number alone.
    #[test]
    fn the_two_spellings_of_home_agree() {
        use crate::catalog::{PhotoQuery, StorageTier};
        let (c, root) = catalog();
        let (_nas, _) = library(&c, &root);

        let summary = c.library_safety_summary().unwrap();
        let listed = c
            .list_photos(&PhotoQuery {
                storage_tier: StorageTier::AtRisk,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(
            listed.len() as i64,
            summary.at_risk,
            "the at-risk count and the at-risk list must be the same photos"
        );

        let stale_listed = c
            .list_photos(&PhotoQuery {
                storage_tier: StorageTier::Stale,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(stale_listed.len() as i64, summary.stale, "same for stale");
    }

    #[test]
    fn a_verified_photo_with_a_moved_on_companion_is_stale_not_safe() {
        // The point of the bucket: the pixels are safe and the edit state is not.
        let (c, root) = catalog();
        let (_nas, photos) = library(&c, &root);
        let stale_id = photos.iter().find(|(s, _)| *s == SafetyStatus::Stale).unwrap().1;

        assert_eq!(c.photo_safety_status(stale_id).unwrap(), SafetyStatus::Stale);
    }

    #[test]
    fn an_uninspected_companion_is_not_counted_as_stale_but_is_counted_as_unchecked() {
        // Freshness is only ever known as of the last scan. A companion nobody has looked
        // at since it was carried is unknown, not fresh — and the panel has to be able to
        // say so rather than implying the stale count is complete.
        let (c, root) = catalog();
        let (nas, _) = library(&c, &root);

        std::fs::write(root.join("later.arw"), b"x").unwrap();
        let id = c.upsert_photo(&root.join("later.arw"), None, 1, 1).unwrap().id;
        std::fs::write(root.join("later.arw.rrdata"), b"edit").unwrap();
        c.backup_photo(id, nas).unwrap();

        let s = c.library_safety_summary().unwrap();
        assert!(s.companions_unchecked >= 1, "the fresh carry is unchecked");
        assert_eq!(
            c.photo_safety_status(id).unwrap(),
            SafetyStatus::Safe,
            "unknown freshness does not by itself make a photo stale"
        );
    }

    #[test]
    fn the_summary_reports_how_long_the_oldest_at_risk_photo_has_waited() {
        let (c, root) = catalog();
        let (_nas, _) = library(&c, &root);

        let s = c.library_safety_summary().unwrap();
        assert!(s.at_risk > 0);
        assert!(s.oldest_at_risk.is_some(), "a count alone does not say how long");
    }

    #[test]
    fn an_empty_library_summarises_to_zeroes_rather_than_erroring() {
        // `SUM` over no rows is NULL, not 0 — a summary that returned an error or a NULL
        // here would break the panel on a brand-new catalog.
        let (c, _root) = catalog();

        let s = c.library_safety_summary().unwrap();

        assert_eq!((s.missing, s.at_risk, s.unverified, s.stale, s.safe), (0, 0, 0, 0, 0));
        assert_eq!(s.oldest_at_risk, None);
    }
}
