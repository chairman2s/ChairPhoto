//! Catalog-wide statistics for the Statistics module.
//!
//! Everything the dashboard shows is folded from ONE pass over `photos_visible`
//! (plus a single join query for top tags): the scan feeds every histogram,
//! marginal, and keeper-analysis crossing at once. The previous shape — one
//! GROUP BY query per panel — cost a full table scan each, ~20 scans per open
//! (~1.7 s on a 165k-photo catalog, all while holding the catalog lock).
//!
//! The raw result is returned as a plain struct; serialisation lives in
//! `commands/graph.rs`.

use super::{Catalog, Result};
use rusqlite::types::ValueRef;
use std::collections::{BTreeMap, HashMap};

/// Time-based stats ignore obviously-bogus capture dates (e.g. "1899-12-31" from
/// scanned film or cameras with an unset clock) — anything before this ISO prefix.
/// Excluded photos are surfaced separately via `invalid_dates`.
const SANE_DATE_FLOOR: &str = "1950";

/// Raw statistics gathered from the catalog in one lock acquisition. The GPUI Statistics
/// module reads it as is. `Default` is the zeroed result of an empty scope.
#[derive(Debug, Clone)]
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
    /// Per-focal-length tallies, focal length ascending (same rows as
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
#[derive(Debug, Clone, PartialEq)]
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
    /// All `None` → whole-catalog stats.
    pub fn catalog_stats(
        &self,
        tag_id: Option<i64>,
        album_id: Option<i64>,
        batch_id: Option<i64>,
    ) -> Result<CatalogStatsRaw> {
        // Build the scope SQL fragment AND-ed into both queries.
        // tag/album/batch ids are i64 — formatting them directly is injection-safe.
        let mut scope_parts: Vec<String> = Vec::new();

        if let Some(tid) = tag_id {
            // Include the tag itself AND all its descendants (mirrors list_photos).
            let ids = self.descendant_tag_ids(tid)?;
            if ids.is_empty() {
                // tag doesn't exist — scope is empty, return zeroed stats
                return Ok(CatalogStatsRaw::default());
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

        // --- top tags: the one query that joins beyond the photos table ------
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

        // --- everything else: one scan, folded in Rust -----------------------
        let mut acc = Accumulator::default();
        {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT p.capture_time, p.camera_model, p.lens, p.focal_length,
                        p.aperture, p.shutter_speed, p.iso, p.rating, p.pick_state
                 FROM photos_visible p WHERE TRUE{scope}"
            ))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                acc.add_row(row)?;
            }
        }

        Ok(acc.finish(top_tags))
    }
}

/// Running tallies for one dimension value during the scan.
#[derive(Default, Clone, Copy)]
struct Tally {
    total: i64,
    decided: i64,
    picked: i64,
    rated: i64,
    hits: i64,
}

impl Tally {
    fn add(&mut self, decided: bool, picked: bool, rated: bool, hit: bool) {
        self.total += 1;
        self.decided += decided as i64;
        self.picked += picked as i64;
        self.rated += rated as i64;
        self.hits += hit as i64;
    }

    fn into_cross<K>(self, key: K) -> CullCross<K> {
        CullCross {
            key,
            total: self.total,
            decided: self.decided,
            picked: self.picked,
            rated: self.rated,
            hits: self.hits,
        }
    }
}

/// Scan-time aggregation state for `catalog_stats`. f64 dimensions are keyed
/// by bit pattern — identical stored REALs have identical bits, so grouping
/// matches what SQL `GROUP BY` did.
#[derive(Default)]
struct Accumulator {
    total_photos: i64,
    with_capture_time: i64,
    invalid_dates: i64,
    timeline: BTreeMap<String, i64>,
    days: HashMap<String, i64>,
    hours: [i64; 24],
    weekdays: [i64; 7],
    ratings: [i64; 6],
    picked: i64,
    rejected: i64,
    by_camera: HashMap<String, Tally>,
    by_lens: HashMap<String, Tally>,
    by_focal: HashMap<u64, Tally>,
    by_iso: HashMap<i64, Tally>,
    by_aperture: HashMap<u64, Tally>,
    by_shutter: HashMap<String, Tally>,
}

impl Accumulator {
    fn add_row(&mut self, row: &rusqlite::Row<'_>) -> rusqlite::Result<()> {
        self.total_photos += 1;

        // Column order matches the SELECT in catalog_stats.
        if let Some(t) = text_ref(row, 0)? {
            if !t.is_empty() {
                if t >= SANE_DATE_FLOOR {
                    self.with_capture_time += 1;
                    bump(&mut self.timeline, t.get(..7).unwrap_or(t));
                    bump(&mut self.days, t.get(..10).unwrap_or(t));
                    // "YYYY-MM-DDTHH:…" — hour at chars 11-12. Garbage counts
                    // as hour 0, as the previous SQL CAST did.
                    let h = t
                        .get(11..13)
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(0);
                    if (0..24).contains(&h) {
                        self.hours[h as usize] += 1;
                    }
                    if let Some((y, m, d)) = parse_ymd(t) {
                        self.weekdays[weekday_sun0(y, m, d)] += 1;
                    }
                } else {
                    self.invalid_dates += 1;
                }
            }
        }

        let rating: i64 = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
        if (0..6).contains(&rating) {
            self.ratings[rating as usize] += 1;
        }
        let pick = text_ref(row, 8)?.unwrap_or("none");
        let decided = pick != "none";
        let picked = pick == "pick";
        if picked {
            self.picked += 1;
        } else if pick == "reject" {
            self.rejected += 1;
        }
        let rated = rating > 0;
        let hit = rating >= 4;

        if let Some(c) = text_ref(row, 1)? {
            if !c.is_empty() {
                tally(&mut self.by_camera, c, decided, picked, rated, hit);
            }
        }
        if let Some(l) = text_ref(row, 2)? {
            if !l.is_empty() {
                tally(&mut self.by_lens, l, decided, picked, rated, hit);
            }
        }
        if let Some(f) = row.get::<_, Option<f64>>(3)? {
            self.by_focal
                .entry(f.to_bits())
                .or_default()
                .add(decided, picked, rated, hit);
        }
        if let Some(a) = row.get::<_, Option<f64>>(4)? {
            if a > 0.0 {
                self.by_aperture
                    .entry(a.to_bits())
                    .or_default()
                    .add(decided, picked, rated, hit);
            }
        }
        if let Some(s) = text_ref(row, 5)? {
            if !s.is_empty() {
                tally(&mut self.by_shutter, s, decided, picked, rated, hit);
            }
        }
        if let Some(i) = row.get::<_, Option<i64>>(6)? {
            if i > 0 {
                self.by_iso
                    .entry(i)
                    .or_default()
                    .add(decided, picked, rated, hit);
            }
        }
        Ok(())
    }

    fn finish(self, top_tags: Vec<(i64, String, i64)>) -> CatalogStatsRaw {
        let first_month = self.timeline.keys().next().cloned();
        let last_month = self.timeline.keys().next_back().cloned();
        let timeline: Vec<(String, i64)> = self.timeline.into_iter().collect();

        let mut top_days: Vec<(String, i64)> = self.days.into_iter().collect();
        top_days.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_days.truncate(3);

        let (cameras, cull_by_camera) = split_str_dim(self.by_camera);
        let (lenses, cull_by_lens) = split_str_dim(self.by_lens);

        let mut focal: Vec<(f64, Tally)> = self
            .by_focal
            .into_iter()
            .map(|(bits, t)| (f64::from_bits(bits), t))
            .collect();
        focal.sort_by(|a, b| a.0.total_cmp(&b.0));
        let focal_lengths = focal.iter().map(|(k, t)| (*k, t.total)).collect();
        let cull_by_focal = focal.into_iter().map(|(k, t)| t.into_cross(k)).collect();

        let mut cull_by_iso: Vec<CullCross<i64>> = self
            .by_iso
            .into_iter()
            .map(|(k, t)| t.into_cross(k))
            .collect();
        cull_by_iso.sort_by_key(|g| g.key);

        let mut cull_by_aperture: Vec<CullCross<f64>> = self
            .by_aperture
            .into_iter()
            .map(|(bits, t)| t.into_cross(f64::from_bits(bits)))
            .collect();
        cull_by_aperture.sort_by(|a, b| a.key.total_cmp(&b.key));

        let cull_by_shutter = parse_shutter_groups(
            self.by_shutter
                .into_iter()
                .map(|(k, t)| t.into_cross(k))
                .collect(),
        );

        CatalogStatsRaw {
            total_photos: self.total_photos,
            with_capture_time: self.with_capture_time,
            first_month,
            last_month,
            timeline,
            hours: self.hours.to_vec(),
            weekdays: self.weekdays.to_vec(),
            top_tags,
            cameras,
            lenses,
            focal_lengths,
            ratings: self.ratings.to_vec(),
            top_days,
            invalid_dates: self.invalid_dates,
            picked: self.picked,
            rejected: self.rejected,
            cull_by_lens,
            cull_by_camera,
            cull_by_focal,
            cull_by_iso,
            cull_by_aperture,
            cull_by_shutter,
        }
    }
}

/// Borrow a TEXT column without allocating; NULL and non-text read as `None`.
fn text_ref<'a>(row: &'a rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<&'a str>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    })
}

/// Count `key` in a string-keyed map, allocating only on first occurrence.
fn bump<M: StrCounter>(map: &mut M, key: &str) {
    map.bump(key);
}

trait StrCounter {
    fn bump(&mut self, key: &str);
}

impl StrCounter for BTreeMap<String, i64> {
    fn bump(&mut self, key: &str) {
        if let Some(c) = self.get_mut(key) {
            *c += 1;
        } else {
            self.insert(key.to_owned(), 1);
        }
    }
}

impl StrCounter for HashMap<String, i64> {
    fn bump(&mut self, key: &str) {
        if let Some(c) = self.get_mut(key) {
            *c += 1;
        } else {
            self.insert(key.to_owned(), 1);
        }
    }
}

/// Tally into a string-keyed dimension, allocating the key only on first sight.
fn tally(
    map: &mut HashMap<String, Tally>,
    key: &str,
    decided: bool,
    picked: bool,
    rated: bool,
    hit: bool,
) {
    if let Some(t) = map.get_mut(key) {
        t.add(decided, picked, rated, hit);
    } else {
        let mut t = Tally::default();
        t.add(decided, picked, rated, hit);
        map.insert(key.to_owned(), t);
    }
}

/// Sort a string dimension count-desc (key-asc ties, deterministic) and split
/// it into the plain marginal and the keeper crossing.
fn split_str_dim(map: HashMap<String, Tally>) -> (Vec<(String, i64)>, Vec<CullCross<String>>) {
    let mut rows: Vec<(String, Tally)> = map.into_iter().collect();
    rows.sort_by(|a, b| b.1.total.cmp(&a.1.total).then_with(|| a.0.cmp(&b.0)));
    let marginal = rows.iter().map(|(k, t)| (k.clone(), t.total)).collect();
    let cross = rows.into_iter().map(|(k, t)| t.into_cross(k)).collect();
    (marginal, cross)
}

/// Parse the `YYYY-MM-DD` prefix; `None` for anything malformed or out of
/// range (mirrors `strftime('%w', …)` returning NULL on invalid dates).
fn parse_ymd(t: &str) -> Option<(i32, u32, u32)> {
    let b = t.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = t.get(0..4)?.parse::<i32>().ok()?;
    let m = t.get(5..7)?.parse::<u32>().ok()?;
    let d = t.get(8..10)?.parse::<u32>().ok()?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Sakamoto's method; 0 = Sunday, matching SQLite `strftime('%w')`.
fn weekday_sun0(y: i32, m: u32, d: u32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if m < 3 { y - 1 } else { y };
    (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d as i32).rem_euclid(7) as usize
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

/// Zeroed statistics — what a scope that resolves to nothing returns
/// (e.g. a `tag_id` that doesn't exist in the catalog).
impl Default for CatalogStatsRaw {
    fn default() -> Self {
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

    #[test]
    fn weekday_matches_strftime_percent_w() {
        // Known anchors: 2026-06-15 is a Monday, 2000-01-01 a Saturday,
        // 2024-02-29 (leap day) a Thursday.
        assert_eq!(weekday_sun0(2026, 6, 15), 1);
        assert_eq!(weekday_sun0(2000, 1, 1), 6);
        assert_eq!(weekday_sun0(2024, 2, 29), 4);
    }

    #[test]
    fn parse_ymd_rejects_what_strftime_rejects() {
        assert_eq!(parse_ymd("2026-06-15T09:30:00"), Some((2026, 6, 15)));
        assert_eq!(parse_ymd("2024-02-29"), Some((2024, 2, 29)));
        assert_eq!(parse_ymd("2023-02-29"), None, "not a leap year");
        assert_eq!(parse_ymd("2026-13-01"), None);
        assert_eq!(parse_ymd("2026-00-10"), None);
        assert_eq!(parse_ymd("2026-06-31"), None);
        assert_eq!(parse_ymd("2026-06"), None, "too short");
        assert_eq!(parse_ymd("2026/06/15"), None, "wrong separators");
    }
}
