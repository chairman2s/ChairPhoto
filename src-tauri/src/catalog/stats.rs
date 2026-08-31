//! Catalog-wide statistics for the Statistics module.
//!
//! All queries filter `missing = 0`; time-based ones additionally require a
//! non-null, non-empty `capture_time`. The raw result is returned as a plain
//! struct; serialisation lives in `commands.rs`.

use super::{Catalog, Result};
use rusqlite::OptionalExtension;

/// Time-based stats ignore obviously-bogus capture dates (e.g. "1899-12-31" from
/// scanned film or cameras with an unset clock) — anything before this ISO prefix.
/// Excluded photos are surfaced separately via `invalid_dates`.
const SANE_DATE_FLOOR: &str = "1950";

/// Shared predicate for time-based queries: present, non-empty, and plausible.
const TIME_OK: &str =
    "capture_time IS NOT NULL AND capture_time != '' AND capture_time >= '1950'";

/// Raw statistics gathered from the catalog in one lock acquisition.
pub struct CatalogStatsRaw {
    pub total_photos: i64,
    pub with_capture_time: i64,
    pub first_month: Option<String>,
    pub last_month: Option<String>,
    /// `(year-month, count)` for every month that has at least one photo,
    /// ascending.
    pub timeline: Vec<(String, i64)>,
    /// Photo count per hour-of-day (index 0-23).
    pub hours: Vec<i64>,
    /// Photo count per weekday (index 0 = Sunday … 6 = Saturday).
    pub weekdays: Vec<i64>,
    /// Top tags (id, full_path, count), up to 15, count descending.
    pub top_tags: Vec<(i64, String, i64)>,
    /// `(camera_model, count)`, count descending.
    pub cameras: Vec<(String, i64)>,
    /// `(lens, count)`, count descending.
    pub lenses: Vec<(String, i64)>,
    /// `(focal_length, count)`, focal_length ascending.
    pub focal_lengths: Vec<(f64, i64)>,
    /// Photo count per rating level, index 0-5.
    pub ratings: Vec<i64>,
    /// The 3 busiest single days: `(YYYY-MM-DD, count)`, count descending.
    pub top_days: Vec<(String, i64)>,
    /// Photos whose capture date is present but implausible (before 1950) —
    /// excluded from all time-based stats above.
    pub invalid_dates: i64,
    /// Photos with `pick_state = 'pick'` in scope.
    pub picked: i64,
    /// Photos with `pick_state = 'reject'` in scope.
    pub rejected: i64,
    /// Per-lens keeper-analysis tallies, total descending.
    pub cull_by_lens: Vec<CullCross<String>>,
    /// Per-camera keeper-analysis tallies, total descending.
    pub cull_by_camera: Vec<CullCross<String>>,
    /// Per-focal-length tallies, focal length ascending (same predicate as
    /// `focal_lengths` so the two panels agree).
    pub cull_by_focal: Vec<CullCross<f64>>,
    /// Per-ISO tallies, ISO ascending. ISO 0 (unknown) excluded.
    pub cull_by_iso: Vec<CullCross<i64>>,
    /// Per-aperture tallies, f-number ascending. Aperture 0 (manual glass) excluded.
    pub cull_by_aperture: Vec<CullCross<f64>>,
    /// Per-shutter-speed tallies keyed by exposure time in SECONDS, ascending.
    /// EXIF text forms ("1/250", "0.5") are parsed and equal durations merged.
    pub cull_by_shutter: Vec<CullCross<f64>>,
}

/// Per-group tallies for keeper analysis, one row per raw dimension value.
/// `total` doubles as the plain distribution (the ISO/aperture/shutter
/// histograms). Bucketing happens in the frontend, mirroring `focal_lengths`.
///
/// Named `cull_*`, not `keeper_*` — "keeper" is taken by burst-stack proposals
/// (`commands/culling.rs::keeper_reason`), which are unrelated.
pub struct CullCross<K> {
    /// Raw grouped value (lens/camera string, focal mm, ISO, f-number, seconds).
    pub key: K,
    /// Visible photos in scope with this value.
    pub total: i64,
    /// `pick_state != 'none'`.
    pub decided: i64,
    /// `pick_state = 'pick'`.
    pub picked: i64,
    /// `rating > 0`.
    pub rated: i64,
    /// `rating >= 4`.
    pub hits: i64,
}

impl Catalog {
    /// Gather all statistics needed by the Statistics module in one pass.
    ///
    /// When `tag_id`, `album_id`, or `batch_id` are set the stats are scoped to
    /// that subset of the catalog (AND-combined when several are set).
    /// `tag_id` includes photos tagged with any descendant tag as well.
    /// All `None` → whole-catalog stats (original behaviour, byte-identical output).
    pub fn catalog_stats(
        &self,
        tag_id: Option<i64>,
        album_id: Option<i64>,
        batch_id: Option<i64>,
    ) -> Result<CatalogStatsRaw> {
        // Build the scope SQL fragment that will be AND-ed into every query.
        // tag/album/batch ids are i64 — formatting them directly is injection-safe.
        let mut scope_parts: Vec<String> = Vec::new();

        if let Some(tid) = tag_id {
            // Include the tag itself AND all its descendants (mirrors list_photos).
            let ids = self.descendant_tag_ids(tid)?;
            if ids.is_empty() {
                // tag doesn't exist — scope is empty, return zeroed stats
                return Ok(empty_stats());
            }
            let id_list = ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            scope_parts.push(format!(
                "p.id IN (SELECT pt.photo_id FROM photo_tags pt WHERE pt.tag_id IN ({id_list}))"
            ));
        }

        if let Some(aid) = album_id {
            scope_parts.push(format!(
                "p.id IN (SELECT photo_id FROM album_photos WHERE album_id = {aid})"
            ));
        }

        if let Some(bid) = batch_id {
            scope_parts.push(format!("p.import_batch_id = {bid}"));
        }

        let scope = if scope_parts.is_empty() {
            String::new()
        } else {
            format!(" AND {}", scope_parts.join(" AND "))
        };

        // --- totals ---------------------------------------------------------
        let total_photos: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM photos_visible p WHERE TRUE{scope}"),
            [],
            |r| r.get(0),
        )?;

        let with_capture_time: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM photos_visible p WHERE {TIME_OK}{scope}"
            ),
            [],
            |r| r.get(0),
        )?;

        let invalid_dates: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM photos_visible p
                 WHERE p.capture_time IS NOT NULL AND p.capture_time != ''
                   AND p.capture_time < '{SANE_DATE_FLOOR}'{scope}"
            ),
            [],
            |r| r.get(0),
        )?;

        let first_month: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "SELECT MIN(substr(p.capture_time, 1, 7)) FROM photos_visible p
                     WHERE {TIME_OK}{scope}"
                ),
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let last_month: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "SELECT MAX(substr(p.capture_time, 1, 7)) FROM photos_visible p
                     WHERE {TIME_OK}{scope}"
                ),
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        // --- timeline -------------------------------------------------------
        let timeline: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT substr(p.capture_time, 1, 7) AS ym, COUNT(*) AS c
                 FROM photos_visible p
                 WHERE {TIME_OK}{scope}
                 GROUP BY ym ORDER BY ym"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- busiest single days ---------------------------------------------
        let top_days: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT substr(p.capture_time, 1, 10) AS d, COUNT(*) AS c
                 FROM photos_visible p
                 WHERE {TIME_OK}{scope}
                 GROUP BY d ORDER BY c DESC LIMIT 3"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- hours ----------------------------------------------------------
        let hours: Vec<i64> = {
            let mut counts = vec![0i64; 24];
            let mut stmt = self.conn.prepare(&format!(
                "SELECT CAST(substr(p.capture_time, 12, 2) AS INTEGER) AS h, COUNT(*) AS c
                 FROM photos_visible p
                 WHERE {TIME_OK}{scope}
                 GROUP BY h"
            ))?;
            let rows: Vec<(i64, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (h, c) in rows {
                if (0..24).contains(&h) {
                    counts[h as usize] = c;
                }
            }
            counts
        };

        // --- weekdays -------------------------------------------------------
        let weekdays: Vec<i64> = {
            let mut counts = vec![0i64; 7];
            let mut stmt = self.conn.prepare(&format!(
                "SELECT CAST(strftime('%w', p.capture_time) AS INTEGER) AS d, COUNT(*) AS c
                 FROM photos_visible p
                 WHERE {TIME_OK}{scope}
                   AND strftime('%w', p.capture_time) IS NOT NULL
                 GROUP BY d"
            ))?;
            let rows: Vec<(Option<i64>, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (d_opt, c) in rows {
                if let Some(d) = d_opt {
                    if (0..7).contains(&d) {
                        counts[d as usize] = c;
                    }
                }
            }
            counts
        };

        // --- top tags -------------------------------------------------------
        // Scope applies to the joined photos alias `p` — top tags WITHIN the scope.
        let top_tags: Vec<(i64, String, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT t.id, t.full_path, COUNT(DISTINCT pt.photo_id) AS c
                 FROM tags t
                 JOIN photo_tags pt ON pt.tag_id = t.id
                 JOIN photos_visible p ON p.id = pt.photo_id
                 WHERE 1=1{scope}
                 GROUP BY t.id HAVING c > 0 ORDER BY c DESC LIMIT 15"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- cameras --------------------------------------------------------
        let cameras: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT p.camera_model, COUNT(*) AS c FROM photos_visible p
                 WHERE p.camera_model IS NOT NULL AND p.camera_model != ''{scope}
                 GROUP BY p.camera_model ORDER BY c DESC"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- lenses ---------------------------------------------------------
        let lenses: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT p.lens, COUNT(*) AS c FROM photos_visible p
                 WHERE p.lens IS NOT NULL AND p.lens != ''{scope}
                 GROUP BY p.lens ORDER BY c DESC"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- focal lengths --------------------------------------------------
        let focal_lengths: Vec<(f64, i64)> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT p.focal_length, COUNT(*) AS c FROM photos_visible p
                 WHERE p.focal_length IS NOT NULL{scope}
                 GROUP BY p.focal_length ORDER BY p.focal_length"
            ))?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };

        // --- ratings --------------------------------------------------------
        let ratings: Vec<i64> = {
            let mut counts = vec![0i64; 6];
            let mut stmt = self.conn.prepare(&format!(
                "SELECT p.rating, COUNT(*) AS c FROM photos_visible p
                 WHERE TRUE{scope}
                 GROUP BY p.rating"
            ))?;
            let rows: Vec<(Option<i64>, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (rating_opt, c) in rows {
                let r = rating_opt.unwrap_or(0);
                if (0..6).contains(&r) {
                    counts[r as usize] = c;
                }
            }
            counts
        };

        // --- pick verdicts ---------------------------------------------------
        // SUM over an empty scope is NULL, hence the COALESCE.
        let (picked, rejected): (i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(CASE WHEN p.pick_state = 'pick' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN p.pick_state = 'reject' THEN 1 ELSE 0 END), 0)
                 FROM photos_visible p WHERE TRUE{scope}"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // --- keeper-analysis crossings ---------------------------------------
        let cull_by_lens = cull_cross::<String>(
            &self.conn,
            "p.lens",
            "p.lens IS NOT NULL AND p.lens != ''",
            "total DESC",
            &scope,
        )?;
        let cull_by_camera = cull_cross::<String>(
            &self.conn,
            "p.camera_model",
            "p.camera_model IS NOT NULL AND p.camera_model != ''",
            "total DESC",
            &scope,
        )?;
        let cull_by_focal = cull_cross::<f64>(
            &self.conn,
            "p.focal_length",
            "p.focal_length IS NOT NULL",
            "k",
            &scope,
        )?;
        let cull_by_iso = cull_cross::<i64>(
            &self.conn,
            "p.iso",
            "p.iso IS NOT NULL AND p.iso > 0",
            "k",
            &scope,
        )?;
        let cull_by_aperture = cull_cross::<f64>(
            &self.conn,
            "p.aperture",
            "p.aperture IS NOT NULL AND p.aperture > 0",
            "k",
            &scope,
        )?;
        let cull_by_shutter = parse_shutter_groups(cull_cross::<String>(
            &self.conn,
            "p.shutter_speed",
            "p.shutter_speed IS NOT NULL AND p.shutter_speed != ''",
            "k",
            &scope,
        )?);

        Ok(CatalogStatsRaw {
            total_photos,
            with_capture_time,
            first_month,
            last_month,
            timeline,
            hours,
            weekdays,
            top_tags,
            cameras,
            lenses,
            focal_lengths,
            ratings,
            top_days,
            invalid_dates,
            picked,
            rejected,
            cull_by_lens,
            cull_by_camera,
            cull_by_focal,
            cull_by_iso,
            cull_by_aperture,
            cull_by_shutter,
        })
    }
}

/// Run one keeper-analysis crossing: group visible photos in scope by
/// `key_expr` and tally the pick/rating counters in a single pass.
/// Per-group SUMs cannot be NULL — GROUP BY only yields non-empty groups.
fn cull_cross<K: rusqlite::types::FromSql>(
    conn: &rusqlite::Connection,
    key_expr: &str,
    present: &str,
    order_by: &str,
    scope: &str,
) -> rusqlite::Result<Vec<CullCross<K>>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_expr} AS k,
                COUNT(*) AS total,
                SUM(CASE WHEN p.pick_state != 'none' THEN 1 ELSE 0 END) AS decided,
                SUM(CASE WHEN p.pick_state = 'pick' THEN 1 ELSE 0 END) AS picked,
                SUM(CASE WHEN p.rating > 0 THEN 1 ELSE 0 END) AS rated,
                SUM(CASE WHEN p.rating >= 4 THEN 1 ELSE 0 END) AS hits
         FROM photos_visible p
         WHERE {present}{scope}
         GROUP BY k ORDER BY {order_by}"
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(CullCross {
            key: r.get(0)?,
            total: r.get(1)?,
            decided: r.get(2)?,
            picked: r.get(3)?,
            rated: r.get(4)?,
            hits: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// exiftool ExposureTime text → seconds: "1/250" → 0.004, "30" → 30.0,
/// "0.5" → 0.5, "2.5\"" → 2.5. None for garbage, "1/0", or non-positive
/// values. (autotags.rs CASTs the same value family in SQL; here we parse
/// properly so fractions bucket correctly.)
fn shutter_seconds(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches('"').trim();
    if let Some((n, d)) = t.split_once('/') {
        let n: f64 = n.trim().parse().ok()?;
        let d: f64 = d.trim().parse().ok()?;
        (n > 0.0 && d > 0.0).then(|| n / d)
    } else {
        t.parse::<f64>().ok().filter(|s| *s > 0.0)
    }
}

/// Convert raw shutter-text groups to seconds, dropping unparseable text and
/// merging groups that name the same duration ("0.5" and "1/2"). Sorted by
/// seconds ascending. Exact f64 equality is safe here: both spellings reach
/// the same value through one division on identical operands.
fn parse_shutter_groups(raw: Vec<CullCross<String>>) -> Vec<CullCross<f64>> {
    let mut parsed: Vec<CullCross<f64>> = raw
        .into_iter()
        .filter_map(|g| {
            shutter_seconds(&g.key).map(|secs| CullCross {
                key: secs,
                total: g.total,
                decided: g.decided,
                picked: g.picked,
                rated: g.rated,
                hits: g.hits,
            })
        })
        .collect();
    parsed.sort_by(|a, b| a.key.partial_cmp(&b.key).expect("shutter seconds are finite"));
    let mut merged: Vec<CullCross<f64>> = Vec::with_capacity(parsed.len());
    for g in parsed {
        match merged.last_mut() {
            Some(last) if last.key == g.key => {
                last.total += g.total;
                last.decided += g.decided;
                last.picked += g.picked;
                last.rated += g.rated;
                last.hits += g.hits;
            }
            _ => merged.push(g),
        }
    }
    merged
}

/// Return a zeroed `CatalogStatsRaw` — used when the scope resolves to nothing
/// (e.g. a `tag_id` that doesn't exist in the catalog).
fn empty_stats() -> CatalogStatsRaw {
    CatalogStatsRaw {
        total_photos: 0,
        with_capture_time: 0,
        first_month: None,
        last_month: None,
        timeline: Vec::new(),
        hours: vec![0i64; 24],
        weekdays: vec![0i64; 7],
        top_tags: Vec::new(),
        cameras: Vec::new(),
        lenses: Vec::new(),
        focal_lengths: Vec::new(),
        ratings: vec![0i64; 6],
        top_days: Vec::new(),
        invalid_dates: 0,
        picked: 0,
        rejected: 0,
        cull_by_lens: Vec::new(),
        cull_by_camera: Vec::new(),
        cull_by_focal: Vec::new(),
        cull_by_iso: Vec::new(),
        cull_by_aperture: Vec::new(),
        cull_by_shutter: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutter_seconds_parses_the_exif_text_forms() {
        assert_eq!(shutter_seconds("1/250"), Some(0.004));
        assert_eq!(shutter_seconds("30"), Some(30.0));
        assert_eq!(shutter_seconds("0.5"), Some(0.5));
        assert_eq!(shutter_seconds("2.5\""), Some(2.5));
        assert_eq!(shutter_seconds("1/0"), None);
        assert_eq!(shutter_seconds("0"), None);
        assert_eq!(shutter_seconds("-1/4"), None);
        assert_eq!(shutter_seconds("abc"), None);
        assert_eq!(shutter_seconds(""), None);
    }

    #[test]
    fn parse_shutter_groups_merges_equal_durations_and_sorts() {
        let cross = |key: &str, total: i64, picked: i64| CullCross {
            key: key.to_string(),
            total,
            decided: total,
            picked,
            rated: total,
            hits: picked,
        };
        let out = parse_shutter_groups(vec![
            cross("1/2", 2, 1),
            cross("garbage", 5, 5),
            cross("1/250", 3, 2),
            cross("0.5", 1, 0),
        ]);
        // "garbage" dropped; "1/2" and "0.5" merged; ascending by seconds.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, 0.004);
        assert_eq!((out[0].total, out[0].picked), (3, 2));
        assert_eq!(out[1].key, 0.5);
        assert_eq!((out[1].total, out[1].decided, out[1].picked), (3, 3, 1));
    }
}
