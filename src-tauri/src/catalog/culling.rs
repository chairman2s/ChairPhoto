//! The photos a culling signal was derived *from* (C6).
//!
//! A `soft-in-burst` badge is a claim about a photo's neighbours: it says this frame is
//! dim compared to the burst it belongs to. Explaining that badge means putting the burst
//! back in front of the reader, so this module answers one question — **which rows would
//! the H15b engine put in this photo's cluster?** — and leaves the flagging rule itself to
//! [`crate::burst::flag_cluster`].
//!
//! ## Why the neighbourhood is reconstructed rather than read back
//!
//! `analyze_burst_sharpness` clusters whatever photo set the caller hands it (typically a
//! selection or a folder) and persists only the resulting flag. The cluster itself is not
//! kept, and the same photo analysed inside a different set can land in a different
//! cluster. So an explanation cannot look the cluster up; it has to rebuild one — and it
//! has to say so when the rebuilt cluster disagrees with the stored flag. That
//! disagreement is a real fact about the catalog (a stale flag), not an inconsistency to
//! paper over.
//!
//! ## The window, and where it stops being exact
//!
//! Clustering splits a capture-time-ordered run wherever the gap between neighbours
//! exceeds `time_gap_secs`, so the cluster containing a photo is bounded by the first such
//! gap on either side. Walking outward from the subject finds that run exactly —
//! *provided it fits inside the rows fetched*. Both bounds (a time span and a row cap) can
//! cut a very long run short; when they do, [`BurstNeighbourhood::truncated`] is set and
//! the caller must disclose it rather than present a partial cluster as a whole one.

use super::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// How the catalog stores `photos.capture_time`: fixed-format local ISO, which makes a
/// string `BETWEEN` a time range (the same assumption `suggest_tags_by_time` relies on).
const CAPTURE_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// How many rows to fetch on each side of the subject before giving up on the walk.
const SIDE_LIMIT: usize = 1000;

/// How far to look on each side, as a multiple of `time_gap_secs`. At the default 15 s gap
/// that is an hour either way — far more than a camera burst, while still letting the
/// query use `idx_photos_capture_time` instead of scanning the table.
const SPAN_GAPS: i64 = 240;

/// One photo in a subject's burst neighbourhood, with the columns an explanation needs.
#[derive(Debug, Clone)]
pub struct CullingNeighbour {
    pub id: i64,
    /// Catalog-root-relative and logical, as everywhere else — never a physical path.
    pub path: String,
    /// Parsed capture time, unix seconds. Always known here: a row whose capture time
    /// cannot be parsed cannot be placed in a time run, so it never enters a
    /// neighbourhood.
    pub capture_ts: i64,
    pub phash: Option<u64>,
    pub rating: i64,
    pub sharpness: Option<f64>,
    pub sharpness_method: Option<String>,
    /// The flag persisted by the last `analyze_burst_sharpness` run, which may have
    /// clustered this photo among a different set of neighbours than the ones here.
    pub burst_flag: Option<String>,
}

/// The maximal run of photos around a subject whose neighbours are all within
/// `time_gap_secs` of each other — the time group the H15b engine would form.
#[derive(Debug, Clone)]
pub struct BurstNeighbourhood {
    /// Members in capture order (`capture_ts`, then `id`), matching how the engine sorts.
    /// Always contains the subject.
    pub members: Vec<CullingNeighbour>,
    /// The run reached the edge of what was fetched, so it may continue beyond these
    /// members. A caller reporting cluster size or rank must say so.
    pub truncated: bool,
}

/// The burst time-group containing `photo_id`, or `None` when the photo has no usable
/// capture time (the engine gives those their own single-photo cluster, so there is no
/// burst to explain) or is not one of the photos the grid lists.
///
/// Considers the same photos the grid lists — visible (present, not trashed) and not
/// stacked under another photo. A camera JPEG sitting under its RAW shares that RAW's capture time and very
/// nearly its hash; counting it would inflate every burst and could crown the derivative
/// over its own original.
pub fn burst_neighbourhood(
    conn: &Connection,
    photo_id: i64,
    time_gap_secs: i64,
) -> Result<Option<BurstNeighbourhood>> {
    let capture: Option<String> = conn
        .query_row(
            "-- includes-hidden: a by-id read of the subject's own capture time, before
             -- the window is built. A hidden subject still falls out below, because
             -- it will not appear in its own neighbourhood.
             SELECT capture_time FROM photos WHERE id = ?1",
            params![photo_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(capture) = capture.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(subject_ts) = parse_capture(&capture) else {
        return Ok(None);
    };

    // A non-positive gap would put every photo in its own cluster; keep the walk sane.
    let gap = time_gap_secs.max(0);
    let span = gap.saturating_mul(SPAN_GAPS).max(gap + 1);
    let lo = format_capture(subject_ts - span);
    let hi = format_capture(subject_ts + span);

    // Two half-windows anchored on the subject rather than one `LIMIT`ed range: an
    // ordinary limit over the whole window returns its *earliest* rows, which for a
    // subject late in a busy hour need not include the subject at all.
    let (before, before_full) = window_side(conn, &lo, &capture, true)?;
    let (after, after_full) = window_side(conn, &capture, &hi, false)?;

    let mut rows: Vec<CullingNeighbour> = before;
    rows.extend(after);
    // The halves overlap on every row sharing the subject's exact capture_time — frames
    // of one burst routinely land in the same second.
    rows.sort_by_key(|n| (n.capture_ts, n.id));
    rows.dedup_by_key(|n| n.id);

    let Some(at) = rows.iter().position(|n| n.id == photo_id) else {
        // The subject is absent from its own window: it is missing, or stacked under
        // another photo, so the grid never shows it in a burst either.
        return Ok(None);
    };

    // Walk outward while each step is within the gap.
    let mut start = at;
    while start > 0 && rows[start].capture_ts - rows[start - 1].capture_ts <= gap {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < rows.len() && rows[end + 1].capture_ts - rows[end].capture_ts <= gap {
        end += 1;
    }

    // The run is known to be complete only where it stopped at a real gap. Where it
    // stopped at the edge of the fetched rows, the next photo was never looked at — either
    // because the row cap cut the side short, or because the time span ends close enough
    // to the last row that a within-gap neighbour could sit just outside it.
    let more_before = start == 0 && (before_full || rows[0].capture_ts - (subject_ts - span) <= gap);
    let more_after =
        end + 1 == rows.len() && (after_full || (subject_ts + span) - rows[end].capture_ts <= gap);

    Ok(Some(BurstNeighbourhood {
        members: rows[start..=end].to_vec(),
        truncated: more_before || more_after,
    }))
}

/// One half of the window. `descending` walks backwards from the subject so the rows
/// nearest it are the ones that survive the limit. Returns the rows in capture order and
/// whether the limit was reached — i.e. whether more rows exist on that side.
fn window_side(
    conn: &Connection,
    lo: &str,
    hi: &str,
    descending: bool,
) -> Result<(Vec<CullingNeighbour>, bool)> {
    let order = if descending {
        "capture_time DESC, id DESC"
    } else {
        "capture_time ASC, id ASC"
    };
    let sql = format!(
        "SELECT id, path, capture_time, phash, rating, sharpness, sharpness_method, burst_flag
         FROM photos_visible
         WHERE capture_time BETWEEN ?1 AND ?2 AND stack_parent_id IS NULL
         ORDER BY {order} LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![lo, hi, SIDE_LIMIT as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut seen = 0usize;
    for row in rows {
        let (id, path, capture_time, phash, rating, sharpness, sharpness_method, burst_flag) = row?;
        seen += 1;
        // A row whose capture_time will not parse cannot sit in a time run — the engine
        // would give it its own cluster — so drop it rather than guess at a position.
        let Some(capture_ts) = capture_time.as_deref().and_then(parse_capture) else {
            continue;
        };
        out.push(CullingNeighbour {
            id,
            path,
            capture_ts,
            phash: phash.map(|v| v as u64),
            rating,
            sharpness,
            sharpness_method,
            burst_flag,
        });
    }
    out.sort_by_key(|n| (n.capture_ts, n.id));
    Ok((out, seen >= SIDE_LIMIT))
}

/// A photo considered for an auto-stack proposal (C3).
#[derive(Debug, Clone)]
pub struct StackCandidate {
    pub id: i64,
    pub path: String,
    /// `None` when the photo has no parseable capture time. The clustering engine puts
    /// those in single-photo clusters, so they never become a proposal — but they are
    /// still counted as considered, because they were.
    pub capture_ts: Option<i64>,
    pub phash: Option<u64>,
    pub rating: i64,
    pub sharpness: Option<f64>,
    pub burst_flag: Option<String>,
    /// Photos already stacked under this one. Stacking it under a keeper re-homes them
    /// onto that keeper (`set_stack_parent` flattens), which the proposal must disclose.
    pub child_count: i64,
}

/// The photos among `photo_ids` that can be proposed for stacking, and how many were
/// dropped because they are already stacked under something.
///
/// Only top-level photos the grid lists are candidates. A photo that is already a stack child is
/// not shown in the grid and is already grouped; proposing to re-group it would silently
/// move it out of the stack its owner put it in — most often the camera JPEG that
/// `pair_raw_jpeg_stacks` paired with its RAW.
pub fn stack_candidates(
    conn: &Connection,
    photo_ids: &[i64],
) -> Result<(Vec<StackCandidate>, usize)> {
    if photo_ids.is_empty() {
        return Ok((Vec::new(), 0));
    }
    // Chunked to stay under SQLite's 999-variable limit, as `burst_inputs` is.
    const CHUNK: usize = 999;
    let mut out: Vec<StackCandidate> = Vec::with_capacity(photo_ids.len());
    let mut found = 0usize;
    for chunk in photo_ids.chunks(CHUNK) {
        let placeholders = (1..=chunk.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, path, capture_time, phash, rating, sharpness, burst_flag,
                    stack_parent_id,
                    -- includes-hidden: the frames already stacked under this one, counted
                    -- so a proposal can disclose what accepting it would re-home. A
                    -- trashed frame is not re-homed, so it does not count here either.
                    (SELECT COUNT(*) FROM photos c
                     WHERE c.stack_parent_id = p.id AND c.trashed_at IS NULL)
             FROM photos_visible p WHERE id IN ({placeholders}) ORDER BY id"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((
                StackCandidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    capture_ts: r.get::<_, Option<String>>(2)?.as_deref().and_then(parse_capture),
                    phash: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    rating: r.get(4)?,
                    sharpness: r.get(5)?,
                    burst_flag: r.get(6)?,
                    child_count: r.get(8)?,
                },
                r.get::<_, Option<i64>>(7)?,
            ))
        })?;
        for row in rows {
            let (candidate, stack_parent_id) = row?;
            found += 1;
            if stack_parent_id.is_none() {
                out.push(candidate);
            }
        }
    }
    let skipped = found - out.len();
    Ok((out, skipped))
}

fn parse_capture(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s, CAPTURE_FMT)
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

fn format_capture(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .naive_utc()
        .format(CAPTURE_FMT)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    /// A real, migrated catalog. `SCHEMA_SQL` alone will not do: `sharpness`, `phash` and
    /// `burst_flag` are added by `migrate_locked` (`catalog/mod.rs:277-295`), so a fixture
    /// built from the DDL lacks the very columns this query selects. `TestTmpDir` keys on
    /// pid and a counter, so the constant tag is still unique per fixture.
    fn catalog() -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new("culling");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    /// Insert a photo shot `t` seconds past 2024-01-01T12:00:00.
    fn shot(c: &Connection, id: i64, t: i64, sharpness: Option<f64>) {
        let base = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        c.execute(
            "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at,
                                updated_at, capture_time, sharpness)
             VALUES(?1, ?2, ?3, 0, 0, 'nef', 0, 0, ?4, ?5)",
            params![
                id,
                format!("uuid-{id}"),
                format!("DSC_{id:04}.NEF"),
                format_capture(base + t),
                sharpness
            ],
        )
        .unwrap();
    }

    fn ids(n: &BurstNeighbourhood) -> Vec<i64> {
        n.members.iter().map(|m| m.id).collect()
    }

    #[test]
    fn the_run_stops_at_the_first_gap_on_each_side() {
        let (cat, _root) = catalog();
        let c = cat.conn();
        // 0s/2s/4s is one run; -100s and +200s are far outside a 15 s gap.
        shot(&c, 1, -100, None);
        shot(&c, 10, 0, None);
        shot(&c, 11, 2, None);
        shot(&c, 12, 4, None);
        shot(&c, 99, 200, None);

        let n = burst_neighbourhood(&c, 11, 15).unwrap().expect("subject has a capture time");
        assert_eq!(ids(&n), vec![10, 11, 12], "only frames chained within the gap belong");
        assert!(!n.truncated, "both sides ended at a real gap");
    }

    #[test]
    fn the_run_chains_across_neighbours_rather_than_measuring_from_the_subject() {
        // 0, 14, 28, 42: every step is inside a 15 s gap but the last frame is 42 s from
        // the subject. A fixed ±window around the subject would cut this burst in half.
        let (cat, _root) = catalog();
        let c = cat.conn();
        for (i, t) in [0i64, 14, 28, 42].iter().enumerate() {
            shot(&c, i as i64 + 1, *t, None);
        }

        let n = burst_neighbourhood(&c, 1, 15).unwrap().unwrap();
        assert_eq!(ids(&n), vec![1, 2, 3, 4]);
    }

    #[test]
    fn frames_in_the_same_second_are_not_lost_between_the_two_half_windows() {
        // Both halves include rows at the subject's exact capture_time; the merge must
        // keep one copy of each — not two, and not none.
        let (cat, _root) = catalog();
        let c = cat.conn();
        for id in 1..=4 {
            shot(&c, id, 0, None);
        }

        let n = burst_neighbourhood(&c, 2, 15).unwrap().unwrap();
        assert_eq!(ids(&n), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_photo_without_a_capture_time_has_no_burst_to_explain() {
        let (cat, _root) = catalog();
        let c = cat.conn();
        c.execute(
            "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
             VALUES(1, 'u1', 'a.NEF', 0, 0, 'nef', 0, 0)",
            [],
        )
        .unwrap();
        assert!(burst_neighbourhood(&c, 1, 15).unwrap().is_none());
    }

    #[test]
    fn missing_and_stacked_photos_stay_out_of_the_run() {
        // The grid lists neither, and `analyze_burst_sharpness` is normally handed what
        // the grid lists — a derivative JPEG would otherwise double every burst frame.
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(50.0));
        shot(&c, 2, 1, Some(60.0));
        shot(&c, 3, 2, Some(70.0));
        c.execute("UPDATE photos SET missing = 1 WHERE id = 2", []).unwrap();
        c.execute("UPDATE photos SET stack_parent_id = 1 WHERE id = 3", []).unwrap();

        let n = burst_neighbourhood(&c, 1, 15).unwrap().unwrap();
        assert_eq!(ids(&n), vec![1], "and the run ends where the listed photos do");
    }

    #[test]
    fn a_chain_longer_than_the_window_is_reported_truncated() {
        // A run longer than SPAN_GAPS × gap cannot be seen whole, and the caller must not
        // read the members it does get as the entire cluster.
        let (cat, _root) = catalog();
        let c = cat.conn();
        for i in 0..(SPAN_GAPS + 20) {
            shot(&c, i + 1, i, None);
        }

        let n = burst_neighbourhood(&c, 1, 1).unwrap().unwrap();
        assert!(
            n.truncated,
            "the chain continues past the fetched window, so the cluster is not fully known"
        );
    }

    // ── stack_candidates (C3) ────────────────────────────────────────────────

    #[test]
    fn a_photo_already_stacked_under_something_is_not_a_candidate_but_is_counted() {
        // It is not in the grid and is already grouped — most often the camera JPEG that
        // `pair_raw_jpeg_stacks` paired with its RAW. Re-proposing it would silently move
        // it out of the stack its owner put it in.
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(100.0));
        shot(&c, 2, 1, Some(90.0));
        c.execute("UPDATE photos SET stack_parent_id = 1 WHERE id = 2", []).unwrap();

        let (candidates, skipped) = stack_candidates(&c, &[1, 2]).unwrap();

        assert_eq!(candidates.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(skipped, 1, "so the caller can say what it left out");
    }

    #[test]
    fn a_candidate_carries_the_count_of_what_is_stacked_under_it() {
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(100.0));
        shot(&c, 2, 1, Some(90.0));
        shot(&c, 3, 2, Some(80.0));
        c.execute("UPDATE photos SET stack_parent_id = 1 WHERE id IN (2, 3)", []).unwrap();

        let (candidates, _) = stack_candidates(&c, &[1, 2, 3]).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].child_count, 2);
    }

    #[test]
    fn a_missing_photo_is_neither_a_candidate_nor_counted_as_skipped() {
        // "Skipped" means "already stacked"; a photo whose file is unreachable was never
        // eligible, and folding it into that count would misreport why.
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(100.0));
        shot(&c, 2, 1, Some(90.0));
        c.execute("UPDATE photos SET missing = 1 WHERE id = 2", []).unwrap();

        let (candidates, skipped) = stack_candidates(&c, &[1, 2]).unwrap();

        assert_eq!(candidates.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn an_id_that_is_not_in_the_catalog_is_simply_absent() {
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(100.0));

        let (candidates, skipped) = stack_candidates(&c, &[1, 999]).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_lone_photo_is_its_own_untruncated_run() {
        let (cat, _root) = catalog();
        let c = cat.conn();
        shot(&c, 1, 0, Some(80.0));

        let n = burst_neighbourhood(&c, 1, 15).unwrap().unwrap();
        assert_eq!(ids(&n), vec![1]);
        assert!(!n.truncated, "nothing was cut off — there is nothing else in the catalog");
    }
}
