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
            &format!("SELECT COUNT(*) FROM photos p WHERE p.missing = 0{scope}"),
            [],
            |r| r.get(0),
        )?;

        let with_capture_time: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM photos p WHERE p.missing = 0 AND {TIME_OK}{scope}"
            ),
            [],
            |r| r.get(0),
        )?;

        let invalid_dates: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM photos p
                 WHERE p.missing = 0 AND p.capture_time IS NOT NULL AND p.capture_time != ''
                   AND p.capture_time < '{SANE_DATE_FLOOR}'{scope}"
            ),
            [],
            |r| r.get(0),
        )?;

        let first_month: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "SELECT MIN(substr(p.capture_time, 1, 7)) FROM photos p
                     WHERE p.missing = 0 AND {TIME_OK}{scope}"
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
                    "SELECT MAX(substr(p.capture_time, 1, 7)) FROM photos p
                     WHERE p.missing = 0 AND {TIME_OK}{scope}"
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
                 FROM photos p
                 WHERE p.missing = 0 AND {TIME_OK}{scope}
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
                 FROM photos p
                 WHERE p.missing = 0 AND {TIME_OK}{scope}
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
                 FROM photos p
                 WHERE p.missing = 0 AND {TIME_OK}{scope}
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
                 FROM photos p
                 WHERE p.missing = 0 AND {TIME_OK}{scope}
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
                 JOIN photos p ON p.id = pt.photo_id AND p.missing = 0
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
                "SELECT p.camera_model, COUNT(*) AS c FROM photos p
                 WHERE p.missing = 0 AND p.camera_model IS NOT NULL AND p.camera_model != ''{scope}
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
                "SELECT p.lens, COUNT(*) AS c FROM photos p
                 WHERE p.missing = 0 AND p.lens IS NOT NULL AND p.lens != ''{scope}
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
                "SELECT p.focal_length, COUNT(*) AS c FROM photos p
                 WHERE p.missing = 0 AND p.focal_length IS NOT NULL{scope}
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
                "SELECT p.rating, COUNT(*) AS c FROM photos p
                 WHERE p.missing = 0{scope}
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
        })
    }
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
    }
}
