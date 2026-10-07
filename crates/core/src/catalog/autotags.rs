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

use super::tag_path::normalize_tag_path;
use super::{Catalog, CatalogError, Result};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

/// The `settings` row claimed by the one pass of [`Catalog::demote_legacy_carriers`].
const LEGACY_CARRIERS_CLAIM: &str = "autotags_legacy_carriers_demoted";

/// Where [`Catalog::rule_tag`] found a rule's tag.
enum RuleTag {
    /// The rule's tag: the key's carrier, or a tag at its path it may take over.
    Found(i64),
    /// Nothing carries the key and the path is free.
    None,
    /// Nothing carries the key and the path holds a tag the rule must not take.
    PathTaken,
}

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

/// A rule the engine cannot turn on because its canonical path is held by a hand tag with
/// photos the rule would not tag (#215, owner decision 2026-10-03): the tag and its photos
/// are left exactly as they are, and the rule stays off for this catalog. The front ends
/// show this as a notice naming the rule and the tag, with what to do about it — rename or
/// merge `blocking_tag_id` to let the rule back in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedAutoTagRule {
    /// Stable rule key (`tags.auto_rule`'s value once the rule does get a tag).
    pub rule: String,
    /// The rule's canonical path — where `blocking_tag_id` sits.
    pub path: String,
    pub blocking_tag_id: i64,
}

impl std::fmt::Display for BlockedAutoTagRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The '{}' rule is off: '{}' already holds photos it would not tag. Rename or \
             merge that tag to let the rule back in.",
            self.rule, self.path
        )
    }
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
    /// command). Each rule's membership is rebuilt from its match query. Returns the rules
    /// left off because their path is held by a hand tag (#215) — the front ends show this
    /// as a notice; [`Self::blocked_auto_tag_rules`] answers the same question without
    /// applying anything, for a panel that just wants to show current state.
    pub fn apply_auto_tags(&self) -> Result<Vec<BlockedAutoTagRule>> {
        let rules = self.auto_tag_rules();
        self.demote_legacy_carriers(&rules)?;
        let mut blocked = Vec::new();
        for rule in &rules {
            if let Some(b) = self.apply_rule(rule)? {
                blocked.push(b);
            }
        }
        Ok(blocked)
    }

    /// Rules currently blocked (#215), read-only: no tag carries the rule's key, and an
    /// ordinary tag at its canonical path holds photos it would not tag. Reflects the
    /// catalog as it stands — not necessarily the result of the last [`Self::apply_auto_tags`],
    /// since a hand edit since then could have freed or taken the path — so a settings panel
    /// can call it on its own schedule instead of caching the last apply's answer.
    pub fn blocked_auto_tag_rules(&self) -> Result<Vec<BlockedAutoTagRule>> {
        let mut out = Vec::new();
        for rule in self.auto_tag_rules() {
            if let Some(b) = self.rule_blocked(&rule)? {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// Whether `rule` is blocked right now: read-only, unlike [`Self::rule_tag`], which may
    /// demote a duplicate carrier left by an earlier engine as a side effect.
    fn rule_blocked(&self, rule: &AutoTagRule) -> Result<Option<BlockedAutoTagRule>> {
        let has_carrier: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tags WHERE auto_rule = ?1)",
            params![rule.rule],
            |r| r.get(0),
        )?;
        if has_carrier {
            return Ok(None);
        }
        match self.path_tag(rule)? {
            RuleTag::PathTaken => Ok(Some(BlockedAutoTagRule {
                rule: rule.rule.to_string(),
                path: rule.path.to_string(),
                blocking_tag_id: self
                    .find_tag_id_by_path(rule.path)?
                    .expect("path_tag found a tag at this path to say PathTaken"),
            })),
            _ => Ok(None),
        }
    }

    /// The one-time step that hands a catalog from the path-keyed engine to the key-keyed one
    /// (review #181 r2 M1). Earlier engines found a rule's tag by its canonical path: a tag
    /// the user renamed or moved kept `auto_rule` but was never rebuilt again, so it took hand
    /// assignments, and the engine made a second carrier at the path. Rebuilding such a tag
    /// now would delete those hand rows. So the first pass under this engine clears
    /// `auto_rule` from every carrier **not** at its rule's canonical path, keeping its rows:
    /// it becomes an ordinary tag the user can edit, delete or merge. A carrier at the path
    /// holds only rule output (the old engine rebuilt it every pass) and stays the rule's tag;
    /// with none there, [`Self::apply_rule`] finds or makes one at the path.
    ///
    /// Claimed once per catalog by the settings row [`LEGACY_CARRIERS_CLAIM`], taken in the
    /// same savepoint as the demotions, so a pass that fails or is rolled back is retried and
    /// two connections never both take it. After it, rename and move work by key: a carrier
    /// the user moves away from the path stays the rule's tag.
    fn demote_legacy_carriers(&self, rules: &[AutoTagRule]) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT autotags_legacy_carriers")?;
        let out = (|| -> Result<()> {
            let claimed = self.conn.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
                params![LEGACY_CARRIERS_CLAIM, now().to_string()],
            )? == 1;
            if !claimed {
                return Ok(());
            }
            for rule in rules {
                self.conn.execute(
                    "UPDATE tags SET auto_rule = NULL WHERE auto_rule = ?1 AND full_path_norm != ?2",
                    params![rule.rule, canonical_norm(rule)?],
                )?;
            }
            Ok(())
        })();
        self.conn.execute_batch(if out.is_ok() {
            "RELEASE autotags_legacy_carriers"
        } else {
            "ROLLBACK TO autotags_legacy_carriers; RELEASE autotags_legacy_carriers"
        })?;
        out
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
    /// Returns the blocked-rule notice (#215) when the path is held by a hand tag; `None`
    /// otherwise, whether or not the rule ended up with a tag.
    fn apply_rule(&self, rule: &AutoTagRule) -> Result<Option<BlockedAutoTagRule>> {
        let any: bool = self.conn.query_row(
            &format!("SELECT EXISTS({})", rule.match_select),
            [],
            |r| r.get(0),
        )?;

        let tag_id = match self.rule_tag(rule)? {
            RuleTag::Found(id) => id,
            // The path holds a tag the rule must not take: the rule goes without one.
            RuleTag::PathTaken => {
                return Ok(Some(BlockedAutoTagRule {
                    rule: rule.rule.to_string(),
                    path: rule.path.to_string(),
                    blocking_tag_id: self
                        .find_tag_id_by_path(rule.path)?
                        .expect("rule_tag found a tag at this path to say PathTaken"),
                }))
            }
            RuleTag::None if !any => return Ok(None),
            RuleTag::None => self.ensure_auto_tag(rule)?,
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
        Ok(None)
    }

    /// The tag `rule` maintains. **A rule's identity is its key (`tags.auto_rule`), not its
    /// path:** a tag carrying the key stays the rule's tag wherever the user renames or moves
    /// it, directly or through an ancestor's rename, move or merge, so no second tag appears
    /// at the rule's path and the renamed one is never left frozen (review #181 M3). Only when
    /// no tag carries the key is the rule's canonical path looked up ([`Self::path_tag`]).
    ///
    /// Carriers left by earlier engines are settled once by
    /// [`Self::demote_legacy_carriers`], after which a rule has at most one. Should two ever
    /// carry a key anyway, the one at the canonical path, else the oldest, keeps it; the
    /// others become ordinary tags with the rows they hold.
    fn rule_tag(&self, rule: &AutoTagRule) -> Result<RuleTag> {
        let ids: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM tags WHERE auto_rule = ?1 ORDER BY full_path_norm = ?2 DESC, id",
            )?;
            let rows = stmt.query_map(params![rule.rule, canonical_norm(rule)?], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        match ids.split_first() {
            Some((&keep, extra)) => {
                for id in extra {
                    self.conn.execute("UPDATE tags SET auto_rule = NULL WHERE id = ?1", params![id])?;
                }
                Ok(RuleTag::Found(keep))
            }
            None => self.path_tag(rule),
        }
    }

    /// The rule's tag when no tag carries its key: the tag at its canonical path, taken over
    /// only when it is an ordinary tag and that loses nothing. Another rule's tag the user
    /// moved there is never taken. Taking a tag over rebuilds its membership, so a tag there
    /// that holds a photo the rule would not tag (made by hand, perhaps after the rule's own
    /// tag was renamed away) is left alone, rows and all, and the rule has no tag until the
    /// path is free or that tag holds only matches. A tag made at the path before the rule
    /// existed, holding only matches or nothing, becomes the rule's tag; one merged or deleted
    /// away is re-created there.
    fn path_tag(&self, rule: &AutoTagRule) -> Result<RuleTag> {
        let Some(id) = self.find_tag_id_by_path(rule.path)? else {
            return Ok(RuleTag::None);
        };
        // Another rule's tag moved onto this path stays that rule's (review #181 r2 L2).
        if self.auto_tag_refusal(id)?.is_some() {
            return Ok(RuleTag::PathTaken);
        }
        let holds_other: bool = self.conn.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM photo_tags WHERE tag_id = ?1
                                 AND photo_id NOT IN (SELECT photo_id FROM ({})))",
                rule.match_select
            ),
            params![id],
            |r| r.get(0),
        )?;
        Ok(if holds_other { RuleTag::PathTaken } else { RuleTag::Found(id) })
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
    /// computed (`'tile'` / `'face'` / `'afpoint'`). Stamped with
    /// `sharpness_indexer::SHARPNESS_BASIS`, as every new score is (#245).
    pub fn set_sharpness(&self, photo_id: i64, score: f64, method: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET sharpness = ?1, sharpness_method = ?2, sharpness_basis = ?3 WHERE id = ?4",
            params![score, method, crate::sharpness_indexer::SHARPNESS_BASIS, photo_id],
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

/// A rule's canonical path in `tags.full_path_norm` form.
fn canonical_norm(rule: &AutoTagRule) -> Result<String> {
    Ok(normalize_tag_path(rule.path).map_err(CatalogError::Tag)?.key())
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
        assert_eq!(carriers(c), vec![auto], "one tag carries the rule");
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

    /// Review #181 r2 L1 (probe P4), what `merge_tags`'s warning promises: an auto-tag merged
    /// away is not made again while nothing matches, and comes back at its path holding every
    /// match — not empty — on the first pass that finds one.
    #[test]
    fn a_merged_away_auto_tag_comes_back_populated_once_something_matches() {
        let (c, _root, long, _fast, auto) = long_and_fast("autotag-merged-away");
        let slow = c.create_tag("Methods/Slow").unwrap();
        crate::catalog::tag_maintenance::merge_tags(&c.conn, &[auto], slow, 1).unwrap();
        assert!(has(&c, long, slow), "the photos moved to the merged tag");

        c.conn.execute("UPDATE photos SET shutter_speed = '1/100' WHERE id = ?1", params![long]).unwrap();
        c.apply_auto_tags().unwrap();
        assert_eq!(c.find_tag_id_by_path(LONG_EXPOSURE).unwrap(), None, "nothing matches: no tag");

        c.conn.execute("UPDATE photos SET shutter_speed = '30' WHERE id = ?1", params![long]).unwrap();
        c.apply_auto_tags().unwrap();
        let back = c.find_tag_id_by_path(LONG_EXPOSURE).unwrap().expect("back at its path");
        assert_eq!(carriers(&c), vec![back]);
        assert!(has(&c, long, back), "holding every match");
    }

    /// #215 (owner decision 2026-10-03): a hand tag at the rule's path holding a photo the
    /// rule would not tag blocks the rule (`RuleTag::PathTaken`) — and this is now reported,
    /// not silent. `apply_auto_tags` returns the block; `blocked_auto_tag_rules` answers the
    /// same question read-only, without applying anything. Freeing the path (merging the
    /// hand tag away) turns the rule back on the next pass.
    #[test]
    fn a_path_taken_by_a_hand_tag_is_reported_as_blocked() {
        let (c, root) = temp_catalog("autotag-blocked");
        let other = photo(&c, &root, "fast.arw", "1/200", "2026-09-01T12:00:00"); // not a match
        let hand = c.create_tag(LONG_EXPOSURE).unwrap();
        c.assign_tag(other, hand).unwrap();

        let long = photo(&c, &root, "long.arw", "30", "2026-09-01T12:00:30"); // would match
        let blocked = c.apply_auto_tags().unwrap();
        assert_eq!(blocked.len(), 1, "{blocked:?}");
        assert_eq!(
            (blocked[0].rule.as_str(), blocked[0].path.as_str(), blocked[0].blocking_tag_id),
            ("long-exposure", LONG_EXPOSURE, hand)
        );

        // The hand tag is untouched: still holds its own photo, never the rule's.
        assert!(has(&c, other, hand), "the hand tag keeps its photo");
        assert!(!has(&c, long, hand), "and never takes the rule's");
        assert_eq!(c.find_tag_id_by_path(LONG_EXPOSURE).unwrap(), Some(hand), "no second tag created at the path");

        // The read-only query agrees, without needing another apply.
        assert_eq!(c.blocked_auto_tag_rules().unwrap(), blocked);

        // Freeing the path (merging the hand tag away) turns the rule back on next pass.
        let elsewhere = c.create_tag("Methods/Hand").unwrap();
        crate::catalog::tag_maintenance::merge_tags(&c.conn, &[hand], elsewhere, 1).unwrap();
        let blocked2 = c.apply_auto_tags().unwrap();
        assert!(blocked2.is_empty(), "{blocked2:?}");
        assert!(c.blocked_auto_tag_rules().unwrap().is_empty());
        let auto = c.find_tag_id_by_path(LONG_EXPOSURE).unwrap().expect("the rule got its tag");
        assert!(has(&c, long, auto), "membership is rebuilt once the rule has a tag");
    }

    /// Review #181 r2 L2 (probe P5): the long-exposure tag moved and renamed onto the
    /// monochrome rule's path, with monochrome carried by no tag. Monochrome's path fallback
    /// must not take it over — even when it holds only monochrome matches — and long-exposure
    /// keeps it.
    #[test]
    fn the_path_fallback_never_takes_another_rules_tag() {
        let (c, _root, long, _fast, auto) = long_and_fast("autotag-other-rule");
        c.set_grayscale(long, true).unwrap();
        let treatment = c.create_tag("Treatment").unwrap();
        c.move_tag(auto, Some(treatment)).unwrap();
        c.rename_tag(auto, "Black & White").unwrap();
        c.apply_auto_tags().unwrap();

        assert_eq!(c.auto_tag_refusal(auto).unwrap().map(|r| r.rule), Some("long-exposure".into()));
        assert_eq!(carriers(&c), vec![auto]);
        assert!(has(&c, long, auto));
        let mono: i64 =
            c.conn.query_row("SELECT COUNT(*) FROM tags WHERE auto_rule = 'monochrome'", [], |r| r.get(0)).unwrap();
        assert_eq!(mono, 0, "monochrome has no tag while another rule's holds its path");
    }

    /// Should two tags ever carry a key after the one-time pass, the one at the rule's path
    /// keeps it and the other becomes an ordinary tag, rows and all.
    #[test]
    fn two_carriers_after_the_pass_collapse_to_the_one_at_the_path() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-dupes");
        c.rename_tag(auto, "Slow Shutter").unwrap();
        let at_path = c.create_tag(LONG_EXPOSURE).unwrap();
        mark_as_old_engine(&c, at_path);
        raw_assign(&c, fast, auto);
        c.apply_auto_tags().unwrap();
        assert_eq!(carriers(&c), vec![at_path]);
        assert!(has(&c, long, at_path) && !has(&c, fast, at_path));
        assert!(has(&c, fast, auto) && has(&c, long, auto), "the demoted tag keeps its rows");
    }

    // ── Carriers an earlier engine left (#181 review r2 M1) ────────────────────────
    //
    // Earlier engines found a rule's tag by path: a renamed carrier kept `auto_rule`, was never
    // rebuilt, took hand rows, and a second carrier appeared at the path. The first pass under
    // the key-keyed engine demotes every carrier not at the path, keeping its rows.

    /// The tags carrying the long-exposure rule, by id.
    fn carriers(c: &Catalog) -> Vec<i64> {
        let mut stmt = c.conn.prepare("SELECT id FROM tags WHERE auto_rule = 'long-exposure' ORDER BY id").unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    /// `c` as an earlier engine left it: the one-time pass not yet taken.
    fn before_the_pass(c: &Catalog) {
        c.conn.execute("DELETE FROM settings WHERE key = ?1", params![LEGACY_CARRIERS_CLAIM]).unwrap();
    }

    /// The carrier an earlier engine made at the rule's path.
    fn mark_as_old_engine(c: &Catalog, tag: i64) {
        c.conn.execute("UPDATE tags SET auto_rule = 'long-exposure' WHERE id = ?1", params![tag]).unwrap();
    }

    /// A row written as the earlier engine let it be: a hand assignment of a frozen carrier,
    /// or the old engine's own output.
    fn raw_assign(c: &Catalog, photo: i64, tag: i64) {
        c.conn
            .execute("INSERT INTO photo_tags (photo_id, tag_id, created_at) VALUES (?1, ?2, 1)", params![photo, tag])
            .unwrap();
    }

    /// Everything the pass could touch: tags with their rule, and memberships.
    fn snapshot(c: &Catalog) -> (Vec<(i64, String, Option<String>)>, Vec<(i64, i64)>) {
        let mut tags = c.conn.prepare("SELECT id, full_path, auto_rule FROM tags ORDER BY id").unwrap();
        let tags = tags.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        let mut rows = c.conn.prepare("SELECT photo_id, tag_id FROM photo_tags ORDER BY 1, 2").unwrap();
        let rows = rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        (tags.collect::<rusqlite::Result<_>>().unwrap(), rows.collect::<rusqlite::Result<_>>().unwrap())
    }

    /// Review P1: the old engine's renamed carrier `auto` ("Slow Shutter") took a hand row (the
    /// fast photo) and a duplicate carrier holds the rule's output at the path. Returns the
    /// duplicate.
    fn renamed_with_a_hand_row_beside_a_duplicate(c: &Catalog, long: i64, fast: i64, auto: i64) -> i64 {
        before_the_pass(c);
        c.rename_tag(auto, "Slow Shutter").unwrap();
        raw_assign(c, fast, auto);
        let dupe = c.create_tag(LONG_EXPOSURE).unwrap();
        mark_as_old_engine(c, dupe);
        raw_assign(c, long, dupe);
        dupe
    }

    #[test]
    fn an_old_engines_renamed_carrier_keeps_its_hand_rows_beside_the_duplicate() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-legacy-p1");
        let dupe = renamed_with_a_hand_row_beside_a_duplicate(&c, long, fast, auto);
        c.apply_auto_tags().unwrap();

        assert_eq!(carriers(&c), vec![dupe], "the carrier at the path stays the rule's tag");
        assert!(has(&c, long, dupe) && !has(&c, fast, dupe));
        assert_eq!(c.auto_tag_refusal(auto).unwrap(), None, "the renamed tag is ordinary");
        assert!(has(&c, fast, auto), "the hand row survives");
        assert!(has(&c, long, auto), "and so does what it held from the rule");
        c.remove_tag(fast, auto).unwrap();
    }

    #[test]
    fn a_lone_renamed_carrier_keeps_its_rows_and_the_rule_starts_again_at_its_path() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-legacy-lone");
        before_the_pass(&c);
        c.rename_tag(auto, "Slow Shutter").unwrap();
        raw_assign(&c, fast, auto);
        c.apply_auto_tags().unwrap();

        assert!(has(&c, fast, auto) && has(&c, long, auto), "the renamed tag keeps every row");
        assert_eq!(c.auto_tag_refusal(auto).unwrap(), None);
        let fresh = c.find_tag_id_by_path(LONG_EXPOSURE).unwrap().expect("re-made at the path");
        assert_eq!(carriers(&c), vec![fresh]);
        assert!(has(&c, long, fresh) && !has(&c, fast, fresh));
    }

    /// Renamed twice under the old engine: two frozen carriers with hand rows, and the third
    /// at the path. Both renamed ones are demoted with their rows.
    #[test]
    fn two_old_renamed_carriers_are_both_demoted() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-legacy-two");
        let second = renamed_with_a_hand_row_beside_a_duplicate(&c, long, fast, auto);
        c.rename_tag(second, "Bulb").unwrap();
        raw_assign(&c, fast, second);
        let third = c.create_tag(LONG_EXPOSURE).unwrap();
        mark_as_old_engine(&c, third);
        raw_assign(&c, long, third);
        c.apply_auto_tags().unwrap();

        assert_eq!(carriers(&c), vec![third]);
        for demoted in [auto, second] {
            assert_eq!(c.auto_tag_refusal(demoted).unwrap(), None);
            assert!(has(&c, fast, demoted) && has(&c, long, demoted), "tag {demoted} keeps its rows");
        }
        assert!(has(&c, long, third) && !has(&c, fast, third));
    }

    /// Review P3 after the pass: the old engine's renamed carrier plus an ordinary tag the user
    /// made at the rule's path and hand-tagged. The renamed one is demoted, and the rule does
    /// not take the hand tag over (that would wipe its row) — until it holds only matches.
    #[test]
    fn the_rule_does_not_take_over_a_hand_tag_at_its_path_that_would_lose_a_row() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-legacy-p3");
        before_the_pass(&c);
        c.rename_tag(auto, "Slow Shutter").unwrap();
        let hand = c.create_tag(LONG_EXPOSURE).unwrap();
        c.assign_tag(fast, hand).unwrap();
        c.apply_auto_tags().unwrap();

        assert!(carriers(&c).is_empty(), "no tag carries the rule");
        assert!(has(&c, fast, hand) && !has(&c, long, hand), "the hand tag is as the user left it");
        assert!(has(&c, long, auto), "the demoted tag keeps its rows");

        c.remove_tag(fast, hand).unwrap();
        c.apply_auto_tags().unwrap();
        assert_eq!(carriers(&c), vec![hand], "an empty tag at the path is taken over");
        assert!(has(&c, long, hand));
    }

    #[test]
    fn a_second_pass_after_the_demotion_changes_nothing() {
        let (c, _root, long, fast, auto) = long_and_fast("autotag-legacy-twice");
        renamed_with_a_hand_row_beside_a_duplicate(&c, long, fast, auto);
        c.apply_auto_tags().unwrap();
        let first = snapshot(&c);
        let claimed = c.get_setting(LEGACY_CARRIERS_CLAIM).unwrap();
        assert!(claimed.is_some(), "the pass is claimed");
        c.apply_auto_tags().unwrap();
        assert_eq!(snapshot(&c), first);
        assert_eq!(c.get_setting(LEGACY_CARRIERS_CLAIM).unwrap(), claimed, "claimed once");
    }

    /// After the pass, a rename moves the rule's tag off its path and it stays the rule's tag:
    /// the demotion ran once, not on every pass.
    #[test]
    fn a_rename_after_the_pass_still_works_by_key() {
        let (c, root, long, fast, auto) = long_and_fast("autotag-legacy-rename");
        let dupe = renamed_with_a_hand_row_beside_a_duplicate(&c, long, fast, auto);
        c.apply_auto_tags().unwrap();
        c.rename_tag(dupe, "Long Shutter").unwrap();
        still_the_rules_tag(&c, &root, dupe, "Technique/Long Shutter", fast);
        assert!(has(&c, fast, auto), "the demoted tag is untouched");
    }
}
