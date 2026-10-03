//! Auto-tags: tags whose membership is computed by a rule and maintained by the
//! system, not assigned by hand. They are otherwise normal tags (they filter,
//! carry synonyms/hashtags, and export). Rules: `monochrome`, `long-exposure`,
//! `panorama`.
//!
//! Each rule is data — a canonical path, a stable rule key, export hashtags, and a
//! SQL `SELECT` returning the matching `photo_id`s. [`Catalog::apply_auto_tags`]
//! rebuilds every rule's `photo_tags` membership from that select, so memberships
//! stay in sync after scans/edits. A rule's tag is the one carrying its key in
//! `tags.auto_rule`, wherever the user has renamed or moved it; the canonical path is used
//! only when no tag carries the key (`rule_tag`). Membership is recomputed in full (simple and
//! correct); optimise to changed photos only if it ever matters.
//!
//! Because the rebuild deletes every row the rule did not derive, the engine owns an
//! auto-tag's membership outright: [`Catalog::assign_tag`] and [`Catalog::remove_tag`]
//! refuse one by hand ([`CatalogError::AutoTag`], #181), and the batch writers
//! [`Catalog::assign_tags`] / [`Catalog::remove_tags`] skip and report them.
//!
//! Monochrome is **pixel-derived**: camera "B&W" flags proved unreliable (e.g. the
//! Sony A7R VI writes a stale `CreativeStyle=B&W` on every frame), so the rule reads
//! `photos.is_grayscale`, a flag computed by sampling the preview during caching.

use super::{Catalog, CatalogError, Result};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

/// A long exposure is a shutter time at or beyond this many seconds.
const LONG_EXPOSURE_SECONDS: f64 = 1.0;

/// A panorama is an image whose long side is at least this multiple of its short
/// side (either orientation).
const PANORAMA_ASPECT: i64 = 2;

/// Why a hand write of an auto-tag was refused: the tag, its path and its rule. Its
/// `Display` is the message a front end shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoTagRefusal {
    pub tag_id: i64,
    pub path: String,
    pub rule: String,
}

impl std::fmt::Display for AutoTagRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' is an auto-tag (rule '{}'): the catalog assigns it from each photo, so it \
             can't be added or removed by hand",
            self.path, self.rule
        )
    }
}

/// What a batch tag write did: how many distinct tags it wrote, and the auto-tags it
/// skipped rather than failing part-way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagBatchOutcome {
    pub tags_written: usize,
    pub skipped: Vec<AutoTagRefusal>,
}

/// One auto-tag rule: a tag to maintain and the photos that belong to it.
struct AutoTagRule {
    /// Stable identifier stored in `tags.auto_rule`.
    rule: &'static str,
    /// Canonical hierarchical path of the tag.
    path: &'static str,
    /// Export-flagged hashtag synonyms created with the tag.
    hashtags: &'static [&'static str],
    /// A `SELECT` returning a single `photo_id` column for the matching photos.
    match_select: String,
}

impl Catalog {
    /// Apply all auto-tags across the catalog. Called after a scan (and exposable as a
    /// command). Each rule's membership is rebuilt from its match query.
    pub fn apply_auto_tags(&self) -> Result<()> {
        for rule in self.auto_tag_rules() {
            self.apply_rule(&rule)?;
        }
        Ok(())
    }

    /// The refusal for a hand write of `tag_id`, or `None` when it is an ordinary tag (or
    /// no tag at all: the write itself reports that).
    pub fn auto_tag_refusal(&self, tag_id: i64) -> Result<Option<AutoTagRefusal>> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row("SELECT full_path, auto_rule FROM tags WHERE id = ?1", params![tag_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        Ok(row.and_then(|(path, rule)| rule.map(|rule| AutoTagRefusal { tag_id, path, rule })))
    }

    /// Whether `path` names an existing auto-tag. Suggestion lists use it to hide a pending
    /// suggestion of one (kept from before #181, or proposed by a model anyway): it could
    /// never be accepted, so it would never leave the list.
    pub fn is_auto_tag_path(&self, path: &str) -> Result<bool> {
        match self.find_tag_id_by_path(path)? {
            Some(id) => Ok(self.auto_tag_refusal(id)?.is_some()),
            None => Ok(false),
        }
    }

    /// `Err(`[`CatalogError::AutoTag`]`)` when `tag_id` is an auto-tag. A verb that changes
    /// other state before tagging (a face confirmation, a suggestion's state) calls this
    /// first, so a refusal leaves nothing half-done.
    pub fn refuse_auto_tag(&self, tag_id: i64) -> Result<()> {
        match self.auto_tag_refusal(tag_id)? {
            Some(refusal) => Err(CatalogError::AutoTag(refusal)),
            None => Ok(()),
        }
    }

    /// Assign every tag to every photo, skipping auto-tags instead of failing part-way
    /// (paste, assign-to-selection). Each auto-tag is reported once in `skipped`.
    pub fn assign_tags(&self, photo_ids: &[i64], tag_ids: &[i64]) -> Result<TagBatchOutcome> {
        self.write_tags(photo_ids, tag_ids, |p, t| self.assign_tag(p, t))
    }

    /// Remove every tag from every photo, skipping auto-tags as [`Self::assign_tags`] does.
    pub fn remove_tags(&self, photo_ids: &[i64], tag_ids: &[i64]) -> Result<TagBatchOutcome> {
        self.write_tags(photo_ids, tag_ids, |p, t| self.remove_tag(p, t))
    }

    fn write_tags(
        &self,
        photo_ids: &[i64],
        tag_ids: &[i64],
        write: impl Fn(i64, i64) -> Result<()>,
    ) -> Result<TagBatchOutcome> {
        let mut out = TagBatchOutcome::default();
        let mut seen = std::collections::HashSet::new();
        for &tag_id in tag_ids {
            if !seen.insert(tag_id) {
                continue;
            }
            if let Some(refusal) = self.auto_tag_refusal(tag_id)? {
                out.skipped.push(refusal);
                continue;
            }
            for &photo_id in photo_ids {
                write(photo_id, tag_id)?;
            }
            out.tags_written += 1;
        }
        Ok(out)
    }

    /// The built-in rule set. `match_select` for monochrome is built from
    /// [`MONO_KEYS`]; the others read promoted columns on `photos` directly.
    fn auto_tag_rules(&self) -> Vec<AutoTagRule> {
        vec![
            AutoTagRule {
                rule: "monochrome",
                path: "Treatment/Black & White",
                hashtags: &["#bnw", "#blackandwhite", "#monochrome"],
                // Pixel-derived: photos sampled as grayscale during caching. Camera
                // metadata is unreliable (see module note).
                match_select: "SELECT id AS photo_id FROM photos WHERE is_grayscale = 1"
                    .to_string(),
            },
            AutoTagRule {
                rule: "long-exposure",
                path: "Technique/Long Exposure",
                hashtags: &["#longexposure", "#longexpo", "#slowshutter"],
                // shutter_speed is stored as text. Fast shutters are fractions
                // ("1/200"); a value with no "/" whose numeric part is >= 1s is a
                // long exposure ("30", "1.3", "2.5\"").
                match_select: format!(
                    "SELECT id AS photo_id FROM photos
                     WHERE shutter_speed IS NOT NULL AND shutter_speed != ''
                       AND shutter_speed NOT LIKE '%/%'
                       AND CAST(shutter_speed AS REAL) >= {LONG_EXPOSURE_SECONDS}"
                ),
            },
            AutoTagRule {
                rule: "panorama",
                path: "Technique/Panorama",
                hashtags: &["#panorama", "#pano"],
                match_select: format!(
                    "SELECT id AS photo_id FROM photos
                     WHERE width > 0 AND height > 0
                       AND (width >= {PANORAMA_ASPECT} * height
                            OR height >= {PANORAMA_ASPECT} * width)"
                ),
            },
        ]
    }

    /// Rebuild one auto-tag's membership from its match query. Creates the tag (with
    /// export hashtags) on first match; if there are no matches and the tag doesn't
    /// exist yet, does nothing (no empty placeholder).
    fn apply_rule(&self, rule: &AutoTagRule) -> Result<()> {
        let any: bool = self.conn.query_row(
            &format!("SELECT EXISTS({})", rule.match_select),
            [],
            |r| r.get(0),
        )?;

        let existing = self.rule_tag(rule)?;
        if !any && existing.is_none() {
            return Ok(());
        }

        let tag_id = match existing {
            Some(id) => id,
            None => self.ensure_auto_tag(rule)?,
        };
        // Make sure the rule + export hashtags are set even on a pre-existing tag.
        self.mark_auto_tag(tag_id, rule.rule)?;

        // Rebuild membership: clear then insert the current matches.
        self.conn
            .execute("DELETE FROM photo_tags WHERE tag_id = ?1", params![tag_id])?;
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id, created_at)
                 SELECT photo_id, ?1, ?2 FROM ({})",
                rule.match_select
            ),
            params![tag_id, now()],
        )?;
        Ok(())
    }

    /// The tag `rule` maintains. **A rule's identity is its key (`tags.auto_rule`), not its
    /// path:** a tag carrying the key stays the rule's tag wherever the user renames or moves
    /// it, directly or through an ancestor's rename, move or merge, so no second tag appears
    /// at the rule's path and the renamed one is never left frozen (review #181 M3). Only when
    /// no tag carries the key is the rule's canonical path looked up (a tag made by hand
    /// before the rule existed becomes the rule's tag; one merged away is re-created there).
    ///
    /// Earlier engines found the tag by path, so a catalog may hold several tags carrying one
    /// key (the renamed one and the duplicate made at the path). The oldest keeps the key;
    /// the others lose it and become ordinary tags with the rows they hold, which the user
    /// can then edit or delete.
    fn rule_tag(&self, rule: &AutoTagRule) -> Result<Option<i64>> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare("SELECT id FROM tags WHERE auto_rule = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![rule.rule], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        match ids.split_first() {
            Some((&keep, extra)) => {
                for id in extra {
                    self.conn.execute("UPDATE tags SET auto_rule = NULL WHERE id = ?1", params![id])?;
                }
                Ok(Some(keep))
            }
            None => self.find_tag_id_by_path(rule.path),
        }
    }

    fn ensure_auto_tag(&self, rule: &AutoTagRule) -> Result<i64> {
        let id = self.create_tag(rule.path)?;
        self.mark_auto_tag(id, rule.rule)?;
        // Export hashtags (synonyms, export-flagged). Idempotent.
        for hashtag in rule.hashtags {
            self.add_term(id, hashtag, None, false, true)?;
        }
        Ok(id)
    }

    fn mark_auto_tag(&self, tag_id: i64, rule: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tags SET auto_rule = ?1 WHERE id = ?2",
            params![rule, tag_id],
        )?;
        Ok(())
    }

    /// The photo's stored B&W flag (pixel- or edit-derived) — see [`Self::set_grayscale`].
    pub fn is_grayscale(&self, photo_id: i64) -> Result<bool> {
        let v: i64 = self.conn.query_row(
            "SELECT COALESCE(is_grayscale, 0) FROM photos WHERE id = ?1",
            params![photo_id],
            |r| r.get(0),
        )?;
        Ok(v != 0)
    }

    /// Record a photo's pixel-derived B&W flag (drives the monochrome auto-tag). The
    /// caller computes it from the decoded preview during caching.
    pub fn set_grayscale(&self, photo_id: i64, is_grayscale: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET is_grayscale = ?1 WHERE id = ?2",
            params![is_grayscale as i64, photo_id],
        )?;
        Ok(())
    }

    /// Record a tiled sharpness score for a photo (H16b). The score is the ~90th-percentile
    /// Laplacian-variance tile from the ~1024–2048px preview; `method` records how it was
    /// computed (`'tile'` / `'face'` / `'afpoint'`). Called by the background indexer and by
    /// the I7b analyzer hook on new imports.
    pub fn set_sharpness(&self, photo_id: i64, score: f64, method: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET sharpness = ?1, sharpness_method = ?2 WHERE id = ?3",
            params![score, method, photo_id],
        )?;
        Ok(())
    }

    /// Record a photo's 64-bit perceptual hash (dHash, H15a). Stored on `photos.phash`
    /// as INTEGER via a bit-preserving `u64 -> i64` reinterpret (the top bit becomes the
    /// sign; that is fine — the hash is only ever compared by Hamming distance, never
    /// ordered). Written by the background `index_phashes` job and the I7b analyzer hook.
    pub fn set_phash(&self, photo_id: i64, phash: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET phash = ?1 WHERE id = ?2",
            params![phash as i64, photo_id],
        )?;
        Ok(())
    }

    /// The photo's stored perceptual hash, or `None` if not yet computed. Reinterprets the
    /// stored i64 bit pattern back to `u64` (inverse of [`set_phash`]).
    pub fn get_phash(&self, photo_id: i64) -> Result<Option<u64>> {
        self.conn
            .query_row(
                "SELECT phash FROM photos WHERE id = ?1",
                params![photo_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(CatalogError::Sqlite)
            .map(|opt| opt.flatten().map(|v| v as u64))
    }

    /// The photo's stored sharpness score and method, or `None` if not yet computed.
    ///
    /// Both `sharpness` and `sharpness_method` are nullable (the photo is unscored until
    /// the background indexer or the import hook writes them). Reading them as non-optional
    /// types would cause `rusqlite` to return `Err(InvalidColumnType)` for NULL rows, which
    /// `.optional()` does not swallow — it only converts `QueryReturnedNoRows`. We therefore
    /// read both columns as `Option<_>` and map `(None, _) | (_, None)` to `Ok(None)`.
    pub fn get_sharpness(&self, photo_id: i64) -> Result<Option<(f64, String)>> {
        self.conn
            .query_row(
                "SELECT sharpness, sharpness_method FROM photos WHERE id = ?1",
                params![photo_id],
                |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(CatalogError::Sqlite)
            .map(|opt| match opt {
                Some((Some(score), Some(method))) => Some((score, method)),
                _ => None,
            })
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::PromotedMetadata;
    use crate::test_support::TestSubPath;

    const LONG_EXPOSURE: &str = "Technique/Long Exposure";

    fn temp_catalog(tag: &str) -> (Catalog, TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    /// A photo with a shutter speed and capture time.
    fn photo(c: &Catalog, root: &TestSubPath, name: &str, shutter: &str, at: &str) -> i64 {
        let id = c.upsert_photo(&root.join(name), None, 1, 1).unwrap().id;
        let meta = PromotedMetadata {
            shutter_speed: Some(shutter.into()),
            capture_time: Some(at.into()),
            ..Default::default()
        };
        c.set_photo_metadata(id, &meta, &[]).unwrap();
        id
    }

    fn has(c: &Catalog, photo_id: i64, tag_id: i64) -> bool {
        c.get_photo_tags(photo_id).unwrap().iter().any(|t| t.id == tag_id)
    }

    /// A 30 s exposure (in the rule) and a 1/200 s one (not), with the rule applied.
    fn long_and_fast(tag: &str) -> (Catalog, TestSubPath, i64, i64, i64) {
        let (c, root) = temp_catalog(tag);
        let long = photo(&c, &root, "long.arw", "30", "2026-09-01T12:00:00");
        let fast = photo(&c, &root, "fast.arw", "1/200", "2026-09-01T12:00:30");
        c.apply_auto_tags().unwrap();
        let auto = c.find_tag_id_by_path(LONG_EXPOSURE).unwrap().unwrap();
        (c, root, long, fast, auto)
    }

    // ── Hand writes of an auto-tag (#181) ─────────────────────────────────────────

    /// #181: a hand assignment the next pass would silently drop is refused instead, so
    /// nothing the user did is lost — and it doesn't count as "recently used".
    #[test]
    fn hand_assignment_of_an_auto_tag_is_refused_and_the_pass_loses_nothing() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-assign");

        let err = c.assign_tag(fast, auto).unwrap_err();
        match &err {
            CatalogError::AutoTag(r) => {
                assert_eq!((r.tag_id, r.path.as_str(), r.rule.as_str()), (auto, LONG_EXPOSURE, "long-exposure"))
            }
            other => panic!("expected an auto-tag refusal, got {other:?}"),
        }
        assert!(err.to_string().contains("can't be added or removed by hand"), "{err}");
        assert!(!has(&c, fast, auto), "the refusal wrote nothing");
        assert!(c.recently_used_tags(10).unwrap().is_empty(), "a refusal is not a use");

        c.apply_auto_tags().unwrap();
        assert!(has(&c, long, auto) && !has(&c, fast, auto), "membership is the rule's");
    }

    /// The other direction of #181: a hand removal the next pass would undo is refused.
    #[test]
    fn hand_removal_of_an_auto_tag_is_refused() {
        let (c, _root, long, _fast, auto) = long_and_fast("autotag-remove");
        assert!(matches!(c.remove_tag(long, auto), Err(CatalogError::AutoTag(_))));
        assert!(has(&c, long, auto));
    }

    /// Batch writes skip an auto-tag, report it once, and still write every other tag to
    /// every photo — they never stop part-way.
    #[test]
    fn batch_writes_skip_auto_tags_and_write_the_rest() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-batch");
        let (trip, people) = (c.create_tag("Events/Trip").unwrap(), c.create_tag("People/Ann").unwrap());

        let out = c.assign_tags(&[long, fast], &[trip, auto, people, auto]).unwrap();
        assert_eq!(out.tags_written, 2);
        assert_eq!(out.skipped.iter().map(|r| r.tag_id).collect::<Vec<_>>(), vec![auto]);
        for p in [long, fast] {
            assert!(has(&c, p, trip) && has(&c, p, people));
        }
        assert!(has(&c, long, auto) && !has(&c, fast, auto));

        let out = c.remove_tags(&[long, fast], &[auto, trip]).unwrap();
        assert_eq!((out.tags_written, out.skipped.len()), (1, 1));
        assert!(!has(&c, long, trip) && !has(&c, fast, trip) && has(&c, long, auto));
    }

    /// Nearby suggestions never offer an auto-tag: a neighbour's "Long Exposure" is not a
    /// tag this photo can take by hand. Its ordinary tags still come through.
    #[test]
    fn nearby_suggestions_exclude_auto_tags() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-nearby");
        let trip = c.create_tag("Events/Trip").unwrap();
        c.assign_tag(long, trip).unwrap();

        let ids: Vec<i64> = c.suggest_tags_by_time(fast, 120).unwrap().iter().map(|t| t.tag.id).collect();
        assert_eq!(ids, vec![trip], "auto-tag {auto} must not be suggested");
    }

    /// A catalog from before #181 can carry `last_used_at` on an auto-tag (a hand assignment
    /// then bumped it); "Recently used" still leaves it out.
    #[test]
    fn recently_used_excludes_an_auto_tag_used_before_the_refusal() {
        let (c, _root, _long, _fast, auto) = long_and_fast("autotag-recent");
        let trip = c.create_tag("Events/Trip").unwrap();
        c.conn.execute("UPDATE tags SET last_used_at = 100 WHERE id IN (?1, ?2)", params![auto, trip]).unwrap();
        let ids: Vec<i64> = c.recently_used_tags(10).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![trip]);
    }

    /// Review #181 L1: hand-assigning a child of an auto-tag doesn't prune the auto-tag's row
    /// as a redundant ancestor — that would be a hand removal the refusal exists to prevent.
    #[test]
    fn a_hand_assigned_child_does_not_prune_the_auto_tag() {
        let (c, _root, long, _fast, auto) = long_and_fast("autotag-prune");
        let nd = c.create_tag("Technique/Long Exposure/ND Filter").unwrap();
        c.assign_tag(long, nd).unwrap();
        assert!(has(&c, long, auto) && has(&c, long, nd));
        assert_eq!(c.tidy_redundant_tags().unwrap(), 0, "the library-wide tidy leaves it too");
        assert!(has(&c, long, auto));
    }

    // ── A rule's identity is its key, not its path (#181 review M3) ───────────────

    /// After a structural change put the long-exposure tag `auto` at `path`: the next pass
    /// keeps `auto` as the rule's only tag (no duplicate at the rule's path), still rebuilds
    /// its membership (a new long exposure joins it), and it still refuses hand writes.
    fn still_the_rules_tag(c: &Catalog, root: &TestSubPath, auto: i64, path: &str, fast: i64) {
        assert_eq!(c.get_tag(auto).unwrap().full_path, path);
        let later = photo(c, root, "later.arw", "15", "2026-09-01T13:00:00");
        c.apply_auto_tags().unwrap();
        let carriers: Vec<i64> = {
            let mut stmt = c.conn.prepare("SELECT id FROM tags WHERE auto_rule = 'long-exposure'").unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(carriers, vec![auto], "one tag carries the rule");
        assert_eq!(c.find_tag_id_by_path(LONG_EXPOSURE).unwrap(), None, "no duplicate at the rule's path");
        assert!(has(c, later, auto), "membership is still rebuilt");
        assert!(matches!(c.assign_tag(fast, auto), Err(CatalogError::AutoTag(_))));
    }

    #[test]
    fn a_renamed_auto_tag_stays_the_rules_tag() {
        let (c, root, _long, fast, auto) = long_and_fast("autotag-rename");
        c.rename_tag(auto, "Slow Shutter").unwrap();
        still_the_rules_tag(&c, &root, auto, "Technique/Slow Shutter", fast);
    }

    #[test]
    fn a_moved_auto_tag_stays_the_rules_tag() {
        let (c, root, _long, fast, auto) = long_and_fast("autotag-move");
        let methods = c.create_tag("Methods").unwrap();
        c.move_tag(auto, Some(methods)).unwrap();
        still_the_rules_tag(&c, &root, auto, "Methods/Long Exposure", fast);
    }

    #[test]
    fn an_auto_tag_under_a_renamed_parent_stays_the_rules_tag() {
        let (c, root, _long, fast, auto) = long_and_fast("autotag-parent-rename");
        let technique = c.find_tag_id_by_path("Technique").unwrap().unwrap();
        c.rename_tag(technique, "Techniques").unwrap();
        still_the_rules_tag(&c, &root, auto, "Techniques/Long Exposure", fast);
    }

    #[test]
    fn an_auto_tag_under_a_merged_parent_stays_the_rules_tag() {
        let (c, root, _long, fast, auto) = long_and_fast("autotag-parent-merge");
        let technique = c.find_tag_id_by_path("Technique").unwrap().unwrap();
        let methods = c.create_tag("Methods").unwrap();
        crate::catalog::tag_maintenance::merge_tags(&c.conn, &[technique], methods, 1).unwrap();
        still_the_rules_tag(&c, &root, auto, "Methods/Long Exposure", fast);
    }

    /// A catalog an earlier engine left with two tags carrying one rule (the renamed tag and
    /// the duplicate it made at the path): the oldest keeps the rule, the other becomes an
    /// ordinary tag the user can edit or delete.
    #[test]
    fn duplicate_rule_tags_from_an_earlier_engine_collapse_to_the_oldest() {
        let (c, root, _long, fast, auto) = long_and_fast("autotag-dupes");
        c.rename_tag(auto, "Slow Shutter").unwrap();
        let dupe = c.create_tag(LONG_EXPOSURE).unwrap();
        c.conn.execute("UPDATE tags SET auto_rule = 'long-exposure' WHERE id = ?1", params![dupe]).unwrap();
        c.apply_auto_tags().unwrap();
        assert_eq!(c.auto_tag_refusal(dupe).unwrap(), None, "the duplicate is an ordinary tag");
        c.assign_tag(fast, dupe).unwrap();
        c.delete_tag(dupe).unwrap();
        still_the_rules_tag(&c, &root, auto, "Technique/Slow Shutter", fast);
    }
}
