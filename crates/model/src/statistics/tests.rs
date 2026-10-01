//! The Statistics logic: the session's generation/epoch rules and cache, the helpers, and the
//! dashboard derived from a payload. The vitest file (`statistics.test.tsx`) tests the
//! component; its payload-level assertions (the keeper sections from its fixture) are ported
//! here one to one in `the_vitest_fixture_renders_the_keeper_sections`, and its view-level
//! cases (scope chip on a scope change, skeleton then cached repaint) in the app crate's
//! `modules::statistics::tests`.

use super::*;

fn cross<K>(key: K, total: i64, decided: i64, picked: i64, rated: i64, hits: i64) -> CullCross<K> {
    CullCross { key, total, decided, picked, rated, hits }
}

/// `statsFixture()` from statistics.test.tsx.
fn vitest_fixture() -> CatalogStatsRaw {
    CatalogStatsRaw {
        total_photos: 5,
        with_capture_time: 5,
        first_month: Some("2024-01".into()),
        last_month: Some("2024-01".into()),
        timeline: vec![("2024-01".into(), 5)],
        picked: 3,
        rejected: 1,
        cull_by_lens: vec![cross("50mm".to_string(), 4, 4, 3, 4, 2)],
        ..CatalogStatsRaw::default()
    }
}

fn scope(tag: i64) -> StatsScope {
    StatsScope { tag_id: Some(tag), ..StatsScope::default() }
}

fn with_total(n: i64) -> CatalogStatsRaw {
    CatalogStatsRaw { total_photos: n, ..CatalogStatsRaw::default() }
}

// --- the session ---------------------------------------------------------------------------

#[test]
fn only_the_newest_request_is_shown_but_a_superseded_one_is_cached() {
    let mut s = StatsSession::new();
    let a = s.request(scope(1));
    let b = s.request(scope(2));
    assert!(s.loading());
    assert!(!s.apply(&a, Ok(with_total(1))), "a superseded answer changes nothing on screen");
    assert!(s.stats().is_none());
    assert_eq!(s.cached(&scope(1)).unwrap().total_photos, 1, "but it is cached for the next open");
    assert!(s.apply(&b, Ok(with_total(2))));
    assert_eq!(s.stats().unwrap().total_photos, 2);
    assert!(!s.loading());
}

#[test]
fn a_cached_scope_repaints_at_once_and_a_new_one_keeps_the_old_figures() {
    let mut s = StatsSession::new();
    let a = s.request(scope(1));
    s.apply(&a, Ok(with_total(10)));
    let b = s.request(scope(2));
    assert_eq!(s.stats().unwrap().total_photos, 10, "no loading flash on a scope change");
    s.apply(&b, Ok(with_total(20)));
    s.request(scope(1));
    assert_eq!(s.stats().unwrap().total_photos, 10, "scope 1 repaints from the cache");
    assert!(s.loading(), "while a fresh read runs");
}

#[test]
fn a_reset_drops_the_cache_and_every_read_in_flight() {
    let mut s = StatsSession::new();
    let a = s.request(scope(1));
    s.apply(&a, Ok(with_total(1)));
    let stale = s.request(scope(2));
    s.reset();
    assert!(s.stats().is_none() && s.cache_len() == 0 && !s.loading());
    assert!(!s.apply(&stale, Ok(with_total(2))));
    assert_eq!(s.cache_len(), 0, "an answer from the closed catalog is not even cached");
    assert!(s.stats().is_none());
}

#[test]
fn an_error_shows_only_for_the_newest_request_and_clears_on_the_next() {
    let mut s = StatsSession::new();
    let a = s.request(scope(1));
    let b = s.request(scope(2));
    s.apply(&a, Err("old".into()));
    assert_eq!(s.error(), None);
    s.apply(&b, Err("No catalog is open".into()));
    assert_eq!(s.error(), Some("No catalog is open"));
    assert!(!s.loading());
    s.request(scope(2));
    assert_eq!(s.error(), None);
}

#[test]
fn the_cache_keeps_sixteen_scopes_evicting_the_oldest_and_replacing_in_place() {
    let mut s = StatsSession::new();
    for tag in 0..STATS_CACHE_CAP as i64 {
        let r = s.request(scope(tag));
        s.apply(&r, Ok(with_total(tag)));
    }
    // Re-setting scope 0 replaces it where it is: it is still the oldest.
    let r = s.request(scope(0));
    s.apply(&r, Ok(with_total(100)));
    assert_eq!(s.cache_len(), STATS_CACHE_CAP);
    let r = s.request(scope(99));
    s.apply(&r, Ok(with_total(99)));
    assert_eq!(s.cache_len(), STATS_CACHE_CAP);
    assert!(s.cached(&scope(0)).is_none(), "the oldest entry went");
    assert!(s.cached(&scope(1)).is_some());
    assert!(s.cached(&scope(99)).is_some());
}

#[test]
fn the_scope_chip_names_the_tag_album_or_batch() {
    assert_eq!(StatsScope::default().label(None), None);
    assert_eq!(scope(4).label(Some("Sunset")).as_deref(), Some("Scoped to tag ‹Sunset›"));
    assert_eq!(scope(4).label(None).as_deref(), Some("Scoped to tag"));
    let album = StatsScope { album_id: Some(1), ..StatsScope::default() };
    assert_eq!(album.label(None).as_deref(), Some("Scoped to current album"));
    let batch = StatsScope { batch_id: Some(1), ..StatsScope::default() };
    assert_eq!(batch.label(None).as_deref(), Some("Scoped to import batch"));
}

// --- helpers ---------------------------------------------------------------------------------

#[test]
fn counts_group_and_percentages_round_like_javascript() {
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(1234567), "1,234,567");
    assert_eq!(grouped(-1000), "-1,000");
    assert_eq!(percent(1, 8), 13, "12.5 rounds up");
    assert_eq!(percent(3, 4), 75);
    assert_eq!(percent(1, 0), 0);
}

#[test]
fn dates_format_as_en_us_printed_them() {
    assert_eq!(fmt_month("2024-01"), "Jan 2024");
    assert_eq!(fmt_month("1999-12"), "Dec 1999");
    assert_eq!(fmt_day("2015-06-12"), "Jun 12, 2015");
}

#[test]
fn the_timeline_fills_gap_months_and_aggregates_years() {
    let sparse = vec![("2023-11".to_string(), 2), ("2024-02".to_string(), 5)];
    let monthly = build_timeline(&sparse, "2023-11", "2024-02");
    let keys: Vec<(&str, i64)> = monthly.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(keys, [("2023-11", 2), ("2023-12", 0), ("2024-01", 0), ("2024-02", 5)]);
    assert_eq!(aggregate_to_years(&monthly), [("2023".to_string(), 2), ("2024".to_string(), 5)]);
    assert_eq!(build_timeline(&[], "0-1", "9999-12").len(), 6000, "the guard caps a bogus span");
}

#[test]
fn monthly_ticks_mark_januaries_and_spans_beyond_180_months_go_yearly() {
    let stats = CatalogStatsRaw {
        first_month: Some("2023-11".into()),
        last_month: Some("2024-02".into()),
        timeline: vec![("2023-11".into(), 2), ("2024-02".into(), 5)],
        ..CatalogStatsRaw::default()
    };
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    assert!(!d.timeline.yearly);
    assert_eq!(d.timeline.heading(), "Photos per month");
    let ticks: Vec<&str> = d.timeline.points.iter().map(|p| p.tick.as_str()).collect();
    assert_eq!(ticks, ["", "", "2024", ""]);
    assert_eq!(d.timeline.points[3].readout, "Feb 2024 · 5 photos");
    assert_eq!(d.timeline.peak_readout(), "Peak 5");

    // 181 months: 2000-01 … 2015-01.
    let long = CatalogStatsRaw {
        first_month: Some("2000-01".into()),
        last_month: Some("2015-01".into()),
        timeline: vec![("2000-01".into(), 1), ("2015-01".into(), 1200)],
        ..CatalogStatsRaw::default()
    };
    let d = Dashboard::derive(&long, RateMetric::Keep);
    assert!(d.timeline.yearly);
    assert_eq!(d.timeline.points.len(), 16);
    // step = ceil(16 / 12) = 2
    let ticks: Vec<&str> = d.timeline.points.iter().take(4).map(|p| p.tick.as_str()).collect();
    assert_eq!(ticks, ["2000", "", "2002", ""]);
    assert_eq!(d.timeline.points[15].readout, "2015 · 1,200 photos");
    assert_eq!(d.timeline.peak, 1200);
}

#[test]
fn buckets_follow_reacts_bounds() {
    assert_eq!(bucket_below(&FL_BUCKETS, 15.9), Some(0));
    assert_eq!(bucket_below(&FL_BUCKETS, 16.), Some(1));
    assert_eq!(bucket_below(&FL_BUCKETS, 600.), Some(7));
    assert_eq!(bucket_below(&ISO_BUCKETS, 100.), Some(0));
    assert_eq!(bucket_below(&ISO_BUCKETS, 141.), Some(1));
    assert_eq!(bucket_below(&APERTURE_BUCKETS, 1.8), Some(1), "f/1.8 is in the f/2 bucket");
    assert_eq!(bucket_below(&APERTURE_BUCKETS, 3.5), Some(3), "f/3.5 is in the f/4 bucket");
    assert_eq!(shutter_bucket(2.), Some(0));
    assert_eq!(shutter_bucket(1. / 250.), Some(3));
    assert_eq!(shutter_bucket(1. / 4000.), Some(5));
    assert_eq!(shutter_bucket(-1.), None);
    assert_eq!(bucket_focal_lengths(&[(10., 1), (24., 2), (28., 3), (400., 4)]), [1, 0, 5, 0, 0, 0, 0, 4]);
}

#[test]
fn edge_zeros_trim_and_interior_zeros_stay() {
    let labels: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
    let (l, v) = trim_zero_by(&labels, &[0, 3, 0, 4, 0], |v| *v == 0);
    assert_eq!((l, v), (vec!["b".to_string(), "c".into(), "d".into()], vec![3, 0, 4]));
    let (l, v) = trim_zero_by(&labels, &[0, 0, 0, 0, 0], |v| *v == 0);
    assert!(l.is_empty() && v.is_empty());
}

#[test]
fn cull_rows_sum_into_buckets_and_unbucketable_rows_drop() {
    let rows = vec![cross(100i64, 2, 2, 1, 1, 0), cross(125, 3, 1, 1, 2, 1), cross(800, 1, 0, 0, 1, 1)];
    let t = aggregate_cull(&rows, |v| bucket_below(&ISO_BUCKETS, v), ISO_BUCKETS.len());
    assert_eq!(t[0], CullTally { total: 5, decided: 3, picked: 2, rated: 3, hits: 1 });
    assert_eq!(t[3].total, 1);
    let shutter = vec![cross(-0.5f64, 9, 9, 9, 9, 9), cross(0.004, 1, 1, 1, 1, 1)];
    let t = aggregate_cull(&shutter, shutter_bucket, SHUTTER_BUCKETS.len());
    assert_eq!(t.iter().map(|t| t.total).sum::<i64>(), 1, "a negative exposure falls out");
}

#[test]
fn the_intensity_ramp_runs_slate_to_accent_to_peak() {
    assert_eq!(intensity_color(0.), 0x334155);
    assert_eq!(intensity_color(0.5), 0x3B82F6);
    assert_eq!(intensity_color(1.), 0x93C5FD);
    assert_eq!(lerp_rgb(0x000000, 0xFFFFFF, 0.5), 0x808080, "127.5 rounds up");
}

// --- the dashboard -------------------------------------------------------------------------

/// statistics.test.tsx "renders the keeper-analysis sections from the stats payload":
/// picked 3 of 4 decided = 75 % in the summary and in the lens row; no ratings → "no rated
/// photos yet"; the exposure cards exist (empty).
#[test]
fn the_vitest_fixture_renders_the_keeper_sections() {
    let d = Dashboard::derive(&vitest_fixture(), RateMetric::Keep);
    assert_eq!(d.photos, "5");
    assert_eq!(d.cull_survival.value, "75%");
    assert_eq!(d.cull_survival.detail, "picked 3 · rejected 1");
    assert_eq!(d.cull_survival.footnote.as_deref(), Some("1 undecided photos not counted"));
    let lens = &d.rate_cards[0];
    assert_eq!(lens.title, "By lens");
    assert_eq!(lens.rows[0].pct(), 75);
    assert_eq!(d.hit_rate.title, "Quality hit rate");
    assert_eq!(d.hit_rate.value, "—");
    assert_eq!(d.hit_rate.detail, "no rated photos yet");
    assert!(d.shutter.is_empty(), "the shutter card renders its empty state");
    assert_eq!(d.date_range, "Jan 2024 – Jan 2024");
}

#[test]
fn the_hit_metric_switches_every_crossing_to_hits_of_rated() {
    let mut stats = vitest_fixture();
    stats.cull_by_camera = vec![
        cross("A".to_string(), 30, 0, 0, 30, 6),
        cross("B".to_string(), 50, 10, 5, 0, 0),
        cross("C".to_string(), 40, 0, 0, 40, 20),
    ];
    let keep = Dashboard::derive(&stats, RateMetric::Keep);
    let rows = &keep.rate_cards[1].rows;
    assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["B"], "zero denominators drop");
    let hit = Dashboard::derive(&stats, RateMetric::Hit);
    let rows = &hit.rate_cards[1].rows;
    assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["C", "A"], "denominator descending");
    assert_eq!((rows[0].num, rows[0].den, rows[0].pct()), (20, 40, 50));
    assert_eq!(hit.rate_cards[0].rows[0].count(), "2/4");
    assert!(hit.rate_cards[0].rows[0].low_n());
    assert_eq!(hit.rate_cards[0].rows[0].title(), "50mm · 2 of 4 · small sample (n=4)");
    assert!(!rows[0].low_n());
}

#[test]
fn gear_crossings_keep_the_twelve_largest() {
    let mut stats = vitest_fixture();
    stats.cull_by_lens = (0..15).map(|i| cross(format!("L{i}"), 100, i + 1, 1, 0, 0)).collect();
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    let rows = &d.rate_cards[0].rows;
    assert_eq!(rows.len(), 12);
    assert_eq!(rows[0].label, "L14");
    assert_eq!(rows[11].label, "L3");
}

#[test]
fn facts_name_the_busiest_day_year_hour_and_the_weekend_share() {
    let mut hours = vec![0; 24];
    hours[23] = 9;
    hours[7] = 9;
    let stats = CatalogStatsRaw {
        first_month: Some("2022-12".into()),
        last_month: Some("2023-01".into()),
        timeline: vec![("2022-12".into(), 7), ("2023-01".into(), 7)],
        top_days: vec![("2015-06-12".into(), 1234)],
        hours,
        // Sun 3, Sat 2 of 10: 50 % on the weekend.
        weekdays: vec![3, 1, 1, 1, 1, 1, 2],
        ..CatalogStatsRaw::default()
    };
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    let values: Vec<&str> = d.facts.iter().map(|f| f.value.as_str()).collect();
    assert_eq!(values, ["Jun 12, 2015 · 1,234", "2022 · 7", "07:00–08:00", "50% on Sat/Sun"]);
    assert_eq!(d.facts[3].label, "Weekend shooter");
    let clock = d.clock.unwrap();
    assert_eq!(clock.peak, "07–08", "the first busiest hour");
    assert_eq!(clock.hours[23].title, "23:00–00:00 · 9 photos");
    assert_eq!(clock.hours[0].outer_radius(), CLOCK_R_INNER + CLOCK_MIN_LEN, "an empty hour keeps a stub");
    assert_eq!(clock.hours[7].outer_radius(), CLOCK_R_MAX);
    assert_eq!(d.weekdays.values, [1, 1, 1, 1, 1, 2, 3], "Monday first");
    assert_eq!(d.weekdays.peak(), Some(6));
    assert_eq!(d.weekdays.titles[6], "Sun · 3 photos");

    let weekday = CatalogStatsRaw { weekdays: vec![1, 2, 2, 2, 2, 2, 0], ..CatalogStatsRaw::default() };
    let d = Dashboard::derive(&weekday, RateMetric::Keep);
    assert_eq!((d.facts[3].label.as_str(), d.facts[3].value.as_str()), ("Weekday shooter", "91% on weekdays"));
    assert_eq!(d.facts[0].value, "—");
    assert_eq!(d.facts[1].value, "—");
    assert_eq!(d.facts[2].value, "—");
    assert!(d.clock.is_none());
    assert_eq!(d.timeline.points.len(), 0);
}

#[test]
fn cameras_make_a_top_five_donut_and_capped_lists() {
    let cameras: Vec<(String, i64)> = (0..14).map(|i| (format!("Cam{i}"), 20 - i)).collect();
    let total: i64 = cameras.iter().map(|c| c.1).sum();
    let stats = CatalogStatsRaw { cameras: cameras.clone(), ..CatalogStatsRaw::default() };
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    let donut = d.donut.unwrap();
    assert_eq!(donut.slices.len(), 6);
    assert_eq!(donut.slices[5].name, "Other");
    assert_eq!(donut.slices[5].count, total - (20 + 19 + 18 + 17 + 16));
    assert_eq!(donut.slices[5].color, DONUT_HUES[5]);
    assert_eq!(donut.top_share, format!("{}%", percent(20, total)));
    assert_eq!(d.camera_count, "14");
    assert_eq!(d.cameras.shown().len(), 12);
    assert_eq!(d.cameras.more().as_deref(), Some("and 2 more…"));
    assert_eq!(d.cameras.fill(&d.cameras.rows[1]), 19. / 20.);
    assert!(Dashboard::derive(&CatalogStatsRaw::default(), RateMetric::Keep).donut.is_none());
}

#[test]
fn top_tags_show_the_leaf_and_link_to_the_tag() {
    let stats = CatalogStatsRaw {
        total_photos: 40,
        top_tags: vec![(7, "Places/Norway/Oslo".into(), 3), (8, "Sunset".into(), 1)],
        ..CatalogStatsRaw::default()
    };
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    let row = &d.tags.rows[0];
    assert_eq!((row.label.as_str(), row.title.as_deref(), row.tag_id), ("Oslo", Some("Places/Norway/Oslo"), Some(7)));
    assert_eq!(d.tags.total, 4, "percentages are of the tag counts");
    assert_eq!(d.tags.pct(row), 75);
    assert!(d.tags.more().is_none(), "top tags are not capped");
    let none = Dashboard::derive(&with_total(40), RateMetric::Keep);
    assert_eq!(none.tags.total, 40, "an empty tag list falls back to the photo count");
}

#[test]
fn exposure_and_rating_bars_carry_reacts_labels_and_titles() {
    let stats = CatalogStatsRaw {
        ratings: vec![5, 0, 0, 1, 2, 3],
        focal_lengths: vec![(35., 2), (100., 1)],
        cull_by_iso: vec![cross(400i64, 3, 0, 0, 0, 0), cross(3200, 1, 0, 0, 0, 0)],
        cull_by_aperture: vec![cross(2.8f64, 2, 0, 0, 0, 0)],
        ..CatalogStatsRaw::default()
    };
    let d = Dashboard::derive(&stats, RateMetric::Keep);
    assert_eq!(d.ratings.labels[0], "—");
    assert_eq!(d.ratings.titles[0], "Unrated · 5 photos");
    assert_eq!(d.ratings.titles[4], "4★ · 2 photos");
    assert_eq!(d.focal.labels, ["35–50", "50–85", "85–135"]);
    assert_eq!(d.focal.values, [2, 0, 1]);
    assert_eq!(d.focal.titles[0], "35–50mm · 2 photos");
    assert_eq!(d.iso.labels, ["400", "800", "1600", "3200"]);
    assert_eq!(d.iso.titles[0], "ISO 400 · 3 photos");
    assert_eq!(d.aperture.labels, ["f/2.8"]);
    assert_eq!(d.hit_rate.value, "83%", "5 of 6 rated are ≥ 4★");
    assert_eq!(d.hit_rate.detail, "5 of 6 rated ≥4★");
    assert_eq!(d.hit_rate.footnote.as_deref(), Some("5 unrated photos not counted"));
    assert_eq!(d.invalid_dates, None);
}
