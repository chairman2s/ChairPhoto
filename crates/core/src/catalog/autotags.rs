//! Auto-tags: tags whose membership is computed by a rule and maintained by the
//! system, not assigned by hand. They are otherwise normal tags (they filter,
//! carry synonyms/hashtags, and export). Rules: `monochrome`, `long-exposure`,
//! `panorama`.
//!
//! Each rule is data — a canonical path, a stable rule key, export hashtags, and a
//! SQL `SELECT` returning the matching `photo_id`s. [`Catalog::apply_auto_tags`]
//! rebuilds every rule's `photo_tags` membership from that select, so memberships
//! stay in sync after scans/edits. Membership is recomputed in full (simple and
//! correct); optimise to changed photos only if it ever matters.
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

        let existing = self.find_tag_id_by_path(rule.path)?;
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
