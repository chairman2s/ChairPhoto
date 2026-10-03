//! The Statistics module's view logic, ported from `src/modules/plugins/statistics.tsx`: the
//! fetch session (which scope's figures are up, the per-scope cache, generation-tagged
//! requests) and the dashboard derived from one `catalog_stats` result — every label,
//! bucket, rate and chart input the view renders. The view only draws it.
//!
//! **Session.** [`StatsSession`] is the `useEffect` + module-scoped `statsCache` pair as a
//! request/answer state machine, like `library::query`: [`StatsSession::request`] hands out a
//! [`StatsRequest`] the caller runs off the UI thread and answers with
//! [`StatsSession::apply`]. Two counters decide what an answer may still touch:
//!
//! - the **generation**, bumped by every request: only the newest request's answer is shown
//!   (React's `alive` flag);
//! - the **epoch**, bumped by a catalog switch or an unload ([`StatsSession::reset`]): an
//!   answer from an older epoch names the closed catalog and is dropped entirely. An answer
//!   that is merely superseded within the epoch still fills the cache — React cached "even
//!   when unmounted mid-flight", which is what makes the next open of that scope instant.
//!
//! **Formatting.** Numbers are grouped as `toLocaleString()` printed them (en-US: `12,345`),
//! percentages round with `Math.round` ([`crate::js_compat::round`]), and the dates are what
//! `toLocaleString("en-US", …)` produced: `fmtMonth` → `Jan 2024`, `fmtDay` → `Jun 12, 2015`
//! (the TS comment on `fmtDay` said `12 Jun 2015`, but en-US puts the month first; the port
//! keeps what the app showed).
//!
//! **Left to the view:** the chart geometry. The React SVG paths (Catmull-Rom area, radial
//! arcs, dash-array donut) become gpui-component charts, so only their inputs live here.

use crate::js_compat::round;
use crate::library::session::LibraryScope;
use chairphoto_core::catalog::{CatalogStatsRaw, CullCross};
use std::collections::BTreeMap;
use std::rc::Rc;

// --- the scope -----------------------------------------------------------------------------

/// What the figures are scoped to: the sidebar's tag, album or import batch (the TS
/// `FilterContext`). A smart-album scope is not part of it — React's filter context never
/// carried one, so the figures then describe the whole catalog, as they did there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StatsScope {
    pub tag_id: Option<i64>,
    pub album_id: Option<i64>,
    pub batch_id: Option<i64>,
}

impl StatsScope {
    /// The Library session's scope as the Statistics view sees it (`setFilterContext`).
    pub fn from_library(scope: &LibraryScope) -> Self {
        StatsScope { tag_id: scope.tag_id, album_id: scope.album_id, batch_id: scope.batch_id }
    }

    /// The scope chip's text, or `None` for the whole catalog. `tag_name` is the scoped tag's
    /// name when known.
    pub fn label(&self, tag_name: Option<&str>) -> Option<String> {
        if self.tag_id.is_some() {
            return Some(match tag_name {
                Some(name) if !name.is_empty() => format!("Scoped to tag ‹{name}›"),
                _ => "Scoped to tag".to_string(),
            });
        }
        if self.album_id.is_some() {
            return Some("Scoped to current album".into());
        }
        if self.batch_id.is_some() {
            return Some("Scoped to import batch".into());
        }
        None
    }
}

/// The chip's second line.
pub const SCOPE_HINT: &str = "Select \"All photos\" in the sidebar to clear";

// --- the session ---------------------------------------------------------------------------

/// Scopes whose last figures are kept (`STATS_CACHE_CAP`); entries are a few KB.
pub const STATS_CACHE_CAP: usize = 16;

/// One `catalog_stats` read to run off the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsRequest {
    pub generation: u64,
    pub epoch: u64,
    pub scope: StatsScope,
}

/// What the dashboard shows and the per-scope cache. See the module docs.
#[derive(Debug, Default)]
pub struct StatsSession {
    /// Insertion order is eviction order (a JS `Map` keeps a re-set key in place).
    cache: Vec<(StatsScope, Rc<CatalogStatsRaw>)>,
    stats: Option<Rc<CatalogStatsRaw>>,
    error: Option<String>,
    generation: u64,
    epoch: u64,
    pending: Option<u64>,
}

impl StatsSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a read for `scope`. The error clears; the figures on screen stay until the
    /// answer lands (no loading flash on a scope change), unless the cache holds `scope`'s,
    /// which go up at once.
    pub fn request(&mut self, scope: StatsScope) -> StatsRequest {
        self.generation += 1;
        self.error = None;
        if let Some(cached) = self.cached(&scope) {
            self.stats = Some(cached);
        }
        self.pending = Some(self.generation);
        StatsRequest { generation: self.generation, epoch: self.epoch, scope }
    }

    /// Land a read's answer. Returns whether anything the view shows changed.
    pub fn apply(&mut self, request: &StatsRequest, result: Result<CatalogStatsRaw, String>) -> bool {
        if request.epoch != self.epoch {
            return false;
        }
        let current = request.generation == self.generation;
        match result {
            Ok(stats) => {
                let stats = Rc::new(stats);
                self.put(request.scope, stats.clone());
                if current {
                    self.stats = Some(stats);
                }
            }
            Err(e) => {
                if current {
                    self.error = Some(e);
                }
            }
        }
        if current {
            self.pending = None;
        }
        current
    }

    /// A catalog switch or the module's unload: drop the cache and the figures, and make
    /// every read in flight unable to land.
    pub fn reset(&mut self) {
        self.epoch += 1;
        self.generation += 1;
        self.cache.clear();
        self.stats = None;
        self.error = None;
        self.pending = None;
    }

    /// The figures on screen (`None`: the skeleton, a cold load).
    pub fn stats(&self) -> Option<&Rc<CatalogStatsRaw>> {
        self.stats.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the newest read has not answered yet.
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn cached(&self, scope: &StatsScope) -> Option<Rc<CatalogStatsRaw>> {
        self.cache.iter().find(|(s, _)| s == scope).map(|(_, v)| v.clone())
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// `statsCachePut`: replace in place, else evict the oldest when full.
    fn put(&mut self, scope: StatsScope, stats: Rc<CatalogStatsRaw>) {
        if let Some(entry) = self.cache.iter_mut().find(|(s, _)| *s == scope) {
            entry.1 = stats;
            return;
        }
        if self.cache.len() >= STATS_CACHE_CAP {
            self.cache.remove(0);
        }
        self.cache.push((scope, stats));
    }
}

// --- helpers ---------------------------------------------------------------------------------

/// `toLocaleString()` for a count (en-US grouping).
pub fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `Math.round(num / den * 100)`, 0 for an empty denominator.
pub fn percent(num: i64, den: i64) -> i64 {
    if den > 0 {
        round(num as f64 / den as f64 * 100.0) as i64
    } else {
        0
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `parseYM`: `"2024-03"` → `(2024, 3)`. Unparseable parts are 0 (JS's `NaN` would have
/// printed `Invalid Date`; the backend never sends one).
pub fn parse_ym(ym: &str) -> (i32, u32) {
    let mut parts = ym.split('-');
    let year = parts.next().and_then(|y| y.parse().ok()).unwrap_or(0);
    let month = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    (year, month)
}

fn month_name(month: u32) -> &'static str {
    MONTHS.get((month as usize).wrapping_sub(1)).copied().unwrap_or("?")
}

/// `fmtMonth`: `"2024-01"` → `Jan 2024`.
pub fn fmt_month(ym: &str) -> String {
    let (year, month) = parse_ym(ym);
    format!("{} {year}", month_name(month))
}

/// `fmtDay`: `"2015-06-12"` → `Jun 12, 2015` (en-US order; see the module docs).
pub fn fmt_day(ymd: &str) -> String {
    let mut parts = ymd.split('-').map(|p| p.parse::<u32>().unwrap_or(0));
    let (y, m, d) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    format!("{} {d}, {y}", month_name(m))
}

fn to_ym(year: i32, month: u32) -> String {
    format!("{year}-{month:02}")
}

/// `buildTimeline`: the sparse months filled in with zeros from `first` to `last`, capped
/// at 6000 months as a guard against a pathological span.
pub fn build_timeline(sparse: &[(String, i64)], first: &str, last: &str) -> Vec<(String, i64)> {
    let counts: BTreeMap<&str, i64> = sparse.iter().map(|(ym, c)| (ym.as_str(), *c)).collect();
    let (mut year, mut month) = parse_ym(first);
    let (end_year, end_month) = parse_ym(last);
    let mut out = Vec::new();
    while (year < end_year || (year == end_year && month <= end_month)) && out.len() < 6000 {
        let ym = to_ym(year, month);
        let count = counts.get(ym.as_str()).copied().unwrap_or(0);
        out.push((ym, count));
        if month == 12 {
            year += 1;
            month = 1;
        } else {
            month += 1;
        }
    }
    out
}

/// `aggregateToYears`: monthly counts summed per year, ascending.
pub fn aggregate_to_years(monthly: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut years: BTreeMap<String, i64> = BTreeMap::new();
    for (ym, c) in monthly {
        let year: String = ym.chars().take(4).collect();
        *years.entry(year).or_default() += c;
    }
    years.into_iter().collect()
}

/// A bucket of a bucketed axis; `bound` is the upper bound (exclusive) or, for shutter
/// speeds, the lower bound (inclusive).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub label: &'static str,
    pub bound: f64,
}

const fn b(label: &'static str, bound: f64) -> Bucket {
    Bucket { label, bound }
}

/// `FL_BUCKETS`: focal length in mm, upper bound exclusive.
pub const FL_BUCKETS: [Bucket; 8] = [
    b("<16", 16.),
    b("16–24", 24.),
    b("24–35", 35.),
    b("35–50", 50.),
    b("50–85", 85.),
    b("85–135", 135.),
    b("135–200", 200.),
    b(">200", f64::INFINITY),
];

/// `ISO_BUCKETS`: upper bound exclusive, at geometric stop midpoints (×√2).
pub const ISO_BUCKETS: [Bucket; 8] = [
    b("≤100", 141.),
    b("200", 283.),
    b("400", 566.),
    b("800", 1131.),
    b("1600", 2263.),
    b("3200", 4526.),
    b("6400", 9051.),
    b(">6400", f64::INFINITY),
];

/// `APERTURE_BUCKETS`: upper bound exclusive, half-stop bounds (nominal × 2^¼) so third-stop
/// values land right.
pub const APERTURE_BUCKETS: [Bucket; 8] = [
    b("≤f/1.4", 1.7),
    b("f/2", 2.4),
    b("f/2.8", 3.4),
    b("f/4", 4.8),
    b("f/5.6", 6.8),
    b("f/8", 9.6),
    b("f/11", 13.5),
    b(">f/11", f64::INFINITY),
];

/// `SHUTTER_BUCKETS`: seconds, slow → fast; the first whose lower bound is ≤ the value wins.
pub const SHUTTER_BUCKETS: [Bucket; 6] = [
    b("≥1s", 1.),
    b("1s–1/15", 1. / 15.),
    b("1/15–1/60", 1. / 60.),
    b("1/60–1/250", 1. / 250.),
    b("1/250–1/1000", 1. / 1000.),
    b("<1/1000", 0.),
];

/// The bucket `v` falls in when bounds are upper and exclusive (`findIndex(v < max)`).
pub fn bucket_below(buckets: &[Bucket], v: f64) -> Option<usize> {
    buckets.iter().position(|bk| v < bk.bound)
}

/// The shutter bucket for `secs` (`findIndex(secs >= min)`); a negative value has none.
pub fn shutter_bucket(secs: f64) -> Option<usize> {
    SHUTTER_BUCKETS.iter().position(|bk| secs >= bk.bound)
}

/// `bucketFocalLengths`: photo counts per [`FL_BUCKETS`] bucket.
pub fn bucket_focal_lengths(raw: &[(f64, i64)]) -> Vec<i64> {
    let mut out = vec![0; FL_BUCKETS.len()];
    for &(focal, count) in raw {
        if let Some(i) = bucket_below(&FL_BUCKETS, focal) {
            out[i] += count;
        }
    }
    out
}

/// `trimZeroBy`: drop leading and trailing items that `is_zero`; interior ones stay.
pub fn trim_zero_by<T: Clone>(labels: &[String], items: &[T], is_zero: impl Fn(&T) -> bool) -> (Vec<String>, Vec<T>) {
    let mut start = 0;
    let mut end = items.len();
    while start < end && is_zero(&items[start]) {
        start += 1;
    }
    while end > start && is_zero(&items[end - 1]) {
        end -= 1;
    }
    (labels[start..end].to_vec(), items[start..end].to_vec())
}

/// Keeper-analysis tallies of one bucket or group (`CullTally`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CullTally {
    pub total: i64,
    pub decided: i64,
    pub picked: i64,
    pub rated: i64,
    pub hits: i64,
}

impl<K> From<&CullCross<K>> for CullTally {
    fn from(c: &CullCross<K>) -> Self {
        CullTally { total: c.total, decided: c.decided, picked: c.picked, rated: c.rated, hits: c.hits }
    }
}

/// A numeric crossing key (ISO is an integer, the rest are REALs), as the JS `number`.
pub trait CullKey: Copy {
    fn as_f64(self) -> f64;
}

impl CullKey for i64 {
    fn as_f64(self) -> f64 {
        self as f64
    }
}

impl CullKey for f64 {
    fn as_f64(self) -> f64 {
        self
    }
}

/// `aggregateCull`: sum rows into `n` buckets by `bucket_of(key)`; `None` drops the row.
pub fn aggregate_cull<K: CullKey>(
    rows: &[CullCross<K>],
    bucket_of: impl Fn(f64) -> Option<usize>,
    n: usize,
) -> Vec<CullTally> {
    let mut out = vec![CullTally::default(); n];
    for r in rows {
        let Some(i) = bucket_of(r.key.as_f64()).filter(|&i| i < n) else { continue };
        let t = &mut out[i];
        t.total += r.total;
        t.decided += r.decided;
        t.picked += r.picked;
        t.rated += r.rated;
        t.hits += r.hits;
    }
    out
}

/// `intensityColor`: `#334155` (low) → `#3B82F6` (accent) → `#93C5FD` (peak), as `0xRRGGBB`.
pub fn intensity_color(t: f64) -> u32 {
    if t <= 0.5 {
        lerp_rgb(0x334155, 0x3B82F6, t / 0.5)
    } else {
        lerp_rgb(0x3B82F6, 0x93C5FD, (t - 0.5) / 0.5)
    }
}

/// `lerpHex`: per channel, `Math.round(a + (b - a) * t)`.
pub fn lerp_rgb(a: u32, b: u32, t: f64) -> u32 {
    let ch = |c: u32, shift: u32| ((c >> shift) & 0xff) as f64;
    [16, 8, 0].iter().fold(0, |acc, &shift| {
        let v = round(ch(a, shift) + (ch(b, shift) - ch(a, shift)) * t) as u32;
        acc | (v.min(255) << shift)
    })
}

/// `DONUT_HUES`: the camera donut's slices (top five + Other).
pub const DONUT_HUES: [u32; 6] = [0x3B82F6, 0x8B5CF6, 0xF59E0B, 0xEC4899, 0x14B8A6, 0x64748B];

/// Weekdays Monday first, and their index in the backend's Sunday-first array.
pub const WEEKDAY_LABELS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const WEEKDAY_ORDER: [usize; 7] = [1, 2, 3, 4, 5, 6, 0];

const RATING_LABELS: [&str; 6] = ["—", "★", "★★", "★★★", "★★★★", "★★★★★"];

/// Rate rows below this denominator are dimmed as small samples, never dropped.
pub const MIN_RATE_DEN: i64 = 20;

/// Gear crossings (lens, camera) show at most this many rows.
const GEAR_RATE_ROWS: usize = 12;

fn hh(h: usize) -> String {
    format!("{:02}", h % 24)
}

// --- the dashboard -------------------------------------------------------------------------

/// Which rate the keeper-analysis crossings show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateMetric {
    /// Cull survival: picked of decided.
    #[default]
    Keep,
    /// ≥ 4★ of rated.
    Hit,
}

/// A rank and rate list's accent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Blue,
    Violet,
    Teal,
}

/// A labelled fact (`st-fact`).
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// The hero chart: photos per month, or per year beyond 180 months.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    pub yearly: bool,
    pub points: Vec<TimelinePoint>,
    /// `Math.max(...values, 1)`: the readout's `Peak`.
    pub peak: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelinePoint {
    /// `"YYYY-MM"` or `"YYYY"`.
    pub key: String,
    pub value: i64,
    /// The axis tick under this point, or `""`.
    pub tick: String,
    /// The hover readout (`fmtValue`).
    pub readout: String,
}

impl Timeline {
    /// The heading: `Photos per month` / `Photos per year`.
    pub fn heading(&self) -> &'static str {
        if self.yearly {
            "Photos per year"
        } else {
            "Photos per month"
        }
    }

    pub fn peak_readout(&self) -> String {
        format!("Peak {}", grouped(self.peak))
    }

    /// The points the chart plots. React draws a lone point as a level line across the whole
    /// chart (`M 0 y L W y`) with its tick and hover dot at `W / 2`; an area chart draws
    /// nothing for one point, so a lone point is plotted three times — the line's two ends
    /// with no tick, and the point itself, tick and all, in the middle. Any other count is
    /// plotted as is.
    pub fn plotted(&self) -> Vec<TimelinePoint> {
        match self.points.as_slice() {
            [only] => {
                let end = TimelinePoint { tick: String::new(), ..only.clone() };
                vec![end.clone(), only.clone(), end]
            }
            points => points.to_vec(),
        }
    }
}

/// Vertical bars (`VBars`): empty `values` (or all zero) render "No data".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bars {
    pub labels: Vec<String>,
    pub values: Vec<i64>,
    /// The hover titles.
    pub titles: Vec<String>,
}

impl Bars {
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(|&v| v == 0)
    }

    /// The tallest bar (the first of equals): `values.indexOf(Math.max(...values, 1))`.
    pub fn peak(&self) -> Option<usize> {
        let max = self.values.iter().copied().max().unwrap_or(0).max(1);
        self.values.iter().position(|&v| v == max)
    }
}

/// The 24-hour clock: one segment per hour, midnight at the top, clockwise.
#[derive(Debug, Clone, PartialEq)]
pub struct Clock {
    pub hours: Vec<ClockSegment>,
    /// The centre: `"HH–HH"` of the busiest hour.
    pub peak: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClockSegment {
    pub hour: usize,
    pub count: i64,
    /// `count / max`, 0–1: the segment's length and colour.
    pub t: f64,
    pub color: u32,
    pub title: String,
}

/// The ring's radii at React's 220 px: segments run from [`CLOCK_R_INNER`] out to at most
/// [`CLOCK_R_MAX`], never shorter than [`CLOCK_MIN_LEN`].
pub const CLOCK_SIZE: f32 = 220.;
pub const CLOCK_R_INNER: f32 = 46.;
pub const CLOCK_R_MAX: f32 = 96.;
pub const CLOCK_MIN_LEN: f32 = 4.;

impl ClockSegment {
    /// `rInner + Math.max(4, t * (rMax - rInner))`.
    pub fn outer_radius(&self) -> f32 {
        CLOCK_R_INNER + CLOCK_MIN_LEN.max(self.t as f32 * (CLOCK_R_MAX - CLOCK_R_INNER))
    }
}

/// The camera donut: the top five cameras and Other.
#[derive(Debug, Clone, PartialEq)]
pub struct Donut {
    pub slices: Vec<DonutSlice>,
    /// The top camera's share, `NN%`.
    pub top_share: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DonutSlice {
    pub name: String,
    pub count: i64,
    pub pct: i64,
    pub color: u32,
    /// `name · count (pct%)`.
    pub title: String,
}

/// A ranked list (`RankList`). `tag_id` makes a row a filter-by-tag link.
#[derive(Debug, Clone, PartialEq)]
pub struct RankRow {
    pub label: String,
    pub count: i64,
    pub title: Option<String>,
    pub tag_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankList {
    pub rows: Vec<RankRow>,
    pub hue: Hue,
    /// What the percentages are of.
    pub total: i64,
    pub cap: Option<usize>,
}

impl RankList {
    pub fn shown(&self) -> &[RankRow] {
        match self.cap {
            Some(cap) => &self.rows[..self.rows.len().min(cap)],
            None => &self.rows,
        }
    }

    /// `and N more…`, when the cap hid rows.
    pub fn more(&self) -> Option<String> {
        let hidden = self.rows.len() - self.shown().len();
        (hidden > 0).then(|| format!("and {} more…", grouped(hidden as i64)))
    }

    /// The bar's fill, 0–1, of the largest count.
    pub fn fill(&self, row: &RankRow) -> f64 {
        let max = self.rows.iter().map(|r| r.count).max().unwrap_or(0).max(1);
        row.count as f64 / max as f64
    }

    pub fn pct(&self, row: &RankRow) -> i64 {
        percent(row.count, self.total)
    }
}

/// A keeper-analysis row: `num` of `den`.
#[derive(Debug, Clone, PartialEq)]
pub struct RateRow {
    pub label: String,
    pub num: i64,
    pub den: i64,
}

impl RateRow {
    pub fn low_n(&self) -> bool {
        self.den < MIN_RATE_DEN
    }

    pub fn pct(&self) -> i64 {
        percent(self.num, self.den)
    }

    pub fn fill(&self) -> f64 {
        if self.den > 0 {
            self.num as f64 / self.den as f64
        } else {
            0.
        }
    }

    pub fn count(&self) -> String {
        format!("{}/{}", grouped(self.num), grouped(self.den))
    }

    pub fn title(&self) -> String {
        let small = if self.low_n() { format!(" · small sample (n={})", self.den) } else { String::new() };
        format!("{} · {} of {}{small}", self.label, grouped(self.num), grouped(self.den))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateCard {
    pub title: &'static str,
    pub rows: Vec<RateRow>,
    pub hue: Hue,
}

/// One of the two keeper summary cards.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub title: &'static str,
    /// `NN%`, or `—` with no denominator.
    pub value: String,
    pub detail: String,
    pub footnote: Option<String>,
}

/// Everything the dashboard renders, from one result and the rate metric.
#[derive(Debug, Clone, PartialEq)]
pub struct Dashboard {
    /// The four stat cards: Photos, Date range, Cameras, Lenses.
    pub photos: String,
    pub date_range: String,
    pub camera_count: String,
    pub lens_count: String,
    /// Busiest day, busiest year, favourite hour, weekend/weekday shooter.
    pub facts: [Fact; 4],
    pub timeline: Timeline,
    /// `N photos with invalid dates excluded`.
    pub invalid_dates: Option<String>,
    /// `None`: no capture hours at all ("No data").
    pub clock: Option<Clock>,
    pub weekdays: Bars,
    /// `None`: no camera data (the donut is left out, the list says "No data").
    pub donut: Option<Donut>,
    pub cameras: RankList,
    pub tags: RankList,
    pub lenses: RankList,
    pub focal: Bars,
    pub ratings: Bars,
    pub iso: Bars,
    pub aperture: Bars,
    pub shutter: Bars,
    pub cull_survival: Summary,
    pub hit_rate: Summary,
    pub metric: RateMetric,
    pub rate_cards: Vec<RateCard>,
}

impl Dashboard {
    pub fn derive(stats: &CatalogStatsRaw, metric: RateMetric) -> Dashboard {
        let date_range = match (&stats.first_month, &stats.last_month) {
            (Some(first), Some(last)) => format!("{} – {}", fmt_month(first), fmt_month(last)),
            _ => "—".to_string(),
        };
        let monthly = match (&stats.first_month, &stats.last_month) {
            (Some(first), Some(last)) => build_timeline(&stats.timeline, first, last),
            _ => stats.timeline.clone(),
        };
        let year_totals = aggregate_to_years(&monthly);
        let timeline = timeline(&monthly, &year_totals);

        // The first year with the most photos wins a tie (`cur > best`).
        let busiest_year = year_totals.iter().fold(None::<&(String, i64)>, |best, cur| match best {
            Some(b) if cur.1 <= b.1 => Some(b),
            _ => Some(cur),
        });

        let weekday = |i: usize| stats.weekdays.get(i).copied().unwrap_or(0);
        let wd_total: i64 = stats.weekdays.iter().sum();
        let weekend_pct = percent(weekday(0) + weekday(6), wd_total);
        let weekend = if weekend_pct >= 45 {
            Fact { label: "Weekend shooter".into(), value: format!("{weekend_pct}% on Sat/Sun") }
        } else {
            Fact { label: "Weekday shooter".into(), value: format!("{}% on weekdays", 100 - weekend_pct) }
        };

        let hour_max = stats.hours.iter().copied().max().unwrap_or(0).max(0);
        let peak_hour = stats.hours.iter().position(|&h| h == hour_max).unwrap_or(0);
        let favourite_hour =
            if hour_max > 0 { format!("{}:00–{}:00", hh(peak_hour), hh(peak_hour + 1)) } else { "—".into() };

        let facts = [
            Fact {
                label: "Busiest day".into(),
                value: stats
                    .top_days
                    .first()
                    .map(|(day, n)| format!("{} · {}", fmt_day(day), grouped(*n)))
                    .unwrap_or_else(|| "—".into()),
            },
            Fact {
                label: "Busiest year".into(),
                value: busiest_year.map(|(y, n)| format!("{y} · {}", grouped(*n))).unwrap_or_else(|| "—".into()),
            },
            Fact { label: "Favorite hour".into(), value: favourite_hour },
            weekend,
        ];

        let wd_values: Vec<i64> = WEEKDAY_ORDER.iter().map(|&i| weekday(i)).collect();
        let weekdays = Bars {
            labels: WEEKDAY_LABELS.iter().map(|s| s.to_string()).collect(),
            titles: WEEKDAY_LABELS.iter().zip(&wd_values).map(|(d, v)| format!("{d} · {} photos", grouped(*v))).collect(),
            values: wd_values,
        };

        let fl_labels: Vec<String> = FL_BUCKETS.iter().map(|b| b.label.to_string()).collect();
        let (labels, values) = trim_zero_by(&fl_labels, &bucket_focal_lengths(&stats.focal_lengths), |v| *v == 0);
        let focal = Bars {
            titles: labels.iter().zip(&values).map(|(l, v)| format!("{l}mm · {} photos", grouped(*v))).collect(),
            labels,
            values,
        };

        let ratings = Bars {
            labels: RATING_LABELS.iter().map(|s| s.to_string()).collect(),
            values: stats.ratings.clone(),
            titles: stats
                .ratings
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let what = if i == 0 { "Unrated".to_string() } else { format!("{i}★") };
                    format!("{what} · {} photos", grouped(*n))
                })
                .collect(),
        };

        // Exposure: raw values bucketed here, as the focal lengths.
        let iso_t = aggregate_cull(&stats.cull_by_iso, |v| bucket_below(&ISO_BUCKETS, v), ISO_BUCKETS.len());
        let ap_t =
            aggregate_cull(&stats.cull_by_aperture, |v| bucket_below(&APERTURE_BUCKETS, v), APERTURE_BUCKETS.len());
        let sh_t = aggregate_cull(&stats.cull_by_shutter, shutter_bucket, SHUTTER_BUCKETS.len());
        let fl_t = aggregate_cull(&stats.cull_by_focal, |v| bucket_below(&FL_BUCKETS, v), FL_BUCKETS.len());

        let labels_of = |bk: &[Bucket]| bk.iter().map(|b| b.label.to_string()).collect::<Vec<_>>();
        let dist = |labels: Vec<String>, tallies: &[CullTally], prefix: &str| {
            let (labels, items) = trim_zero_by(&labels, tallies, |t| t.total == 0);
            Bars {
                titles: labels
                    .iter()
                    .zip(&items)
                    .map(|(l, t)| format!("{prefix}{l} · {} photos", grouped(t.total)))
                    .collect(),
                values: items.iter().map(|t| t.total).collect(),
                labels,
            }
        };
        let iso = dist(labels_of(&ISO_BUCKETS), &iso_t, "ISO ");
        let aperture = dist(labels_of(&APERTURE_BUCKETS), &ap_t, "");
        let shutter = dist(labels_of(&SHUTTER_BUCKETS), &sh_t, "");

        // Keeper summary: pick/reject and rating are independent axes, each rate over its
        // own denominator (decided / rated).
        let decided = stats.picked + stats.rejected;
        let undecided = stats.total_photos - decided;
        let rated: i64 = stats.ratings.iter().skip(1).sum();
        let rating = |i: usize| stats.ratings.get(i).copied().unwrap_or(0);
        let hits = rating(4) + rating(5);
        let unrated = rating(0);
        let rate_value = |num: i64, den: i64| if den > 0 { format!("{}%", percent(num, den)) } else { "—".into() };
        let cull_survival = Summary {
            title: "Cull survival",
            value: rate_value(stats.picked, decided),
            detail: if decided > 0 {
                format!("picked {} · rejected {}", grouped(stats.picked), grouped(stats.rejected))
            } else {
                "no pick/reject decisions yet".into()
            },
            footnote: (undecided > 0).then(|| format!("{} undecided photos not counted", grouped(undecided))),
        };
        let hit_rate = Summary {
            title: "Quality hit rate",
            value: rate_value(hits, rated),
            detail: if rated > 0 {
                format!("{} of {} rated ≥4★", grouped(hits), grouped(rated))
            } else {
                "no rated photos yet".into()
            },
            footnote: (unrated > 0).then(|| format!("{} unrated photos not counted", grouped(unrated))),
        };

        let rate = |t: &CullTally| match metric {
            RateMetric::Keep => (t.picked, t.decided),
            RateMetric::Hit => (t.hits, t.rated),
        };
        // A zero denominator says nothing about a rate: those rows are dropped.
        let bucket_rows = |labels: Vec<String>, tallies: &[CullTally]| -> Vec<RateRow> {
            labels
                .into_iter()
                .zip(tallies)
                .map(|(label, t)| {
                    let (num, den) = rate(t);
                    RateRow { label, num, den }
                })
                .filter(|r| r.den > 0)
                .collect()
        };
        let gear_rows = |rows: &[CullCross<String>]| -> Vec<RateRow> {
            let mut out: Vec<RateRow> = rows
                .iter()
                .map(|r| {
                    let (num, den) = rate(&CullTally::from(r));
                    RateRow { label: r.key.clone(), num, den }
                })
                .filter(|r| r.den > 0)
                .collect();
            out.sort_by(|a, b| b.den.cmp(&a.den)); // stable, as `Array.prototype.sort`
            out.truncate(GEAR_RATE_ROWS);
            out
        };
        let suffixed = |bk: &[Bucket], pre: &str, post: &str| {
            bk.iter().map(|b| format!("{pre}{}{post}", b.label)).collect::<Vec<_>>()
        };
        let rate_cards = vec![
            RateCard { title: "By lens", rows: gear_rows(&stats.cull_by_lens), hue: Hue::Blue },
            RateCard { title: "By camera", rows: gear_rows(&stats.cull_by_camera), hue: Hue::Violet },
            RateCard { title: "By focal length", rows: bucket_rows(suffixed(&FL_BUCKETS, "", "mm"), &fl_t), hue: Hue::Teal },
            RateCard { title: "By ISO", rows: bucket_rows(suffixed(&ISO_BUCKETS, "ISO ", ""), &iso_t), hue: Hue::Blue },
            RateCard {
                title: "By aperture",
                rows: bucket_rows(labels_of(&APERTURE_BUCKETS), &ap_t),
                hue: Hue::Violet,
            },
            RateCard {
                title: "By shutter speed",
                rows: bucket_rows(labels_of(&SHUTTER_BUCKETS), &sh_t),
                hue: Hue::Teal,
            },
        ];

        let tag_sum: i64 = stats.top_tags.iter().map(|t| t.2).sum();
        let tags = RankList {
            rows: stats
                .top_tags
                .iter()
                .map(|(id, path, count)| RankRow {
                    label: path.rsplit('/').next().unwrap_or(path).to_string(),
                    count: *count,
                    title: Some(path.clone()),
                    tag_id: Some(*id),
                })
                .collect(),
            hue: Hue::Blue,
            // `reduce(…) || totalPhotos`
            total: if tag_sum != 0 { tag_sum } else { stats.total_photos },
            cap: None,
        };
        let plain = |rows: &[(String, i64)], hue: Hue| RankList {
            rows: rows.iter().map(|(n, c)| RankRow { label: n.clone(), count: *c, title: None, tag_id: None }).collect(),
            hue,
            total: rows.iter().map(|r| r.1).sum(),
            cap: Some(12),
        };

        Dashboard {
            photos: grouped(stats.total_photos),
            date_range,
            camera_count: grouped(stats.cameras.len() as i64),
            lens_count: grouped(stats.lenses.len() as i64),
            facts,
            timeline,
            invalid_dates: (stats.invalid_dates > 0)
                .then(|| format!("{} photos with invalid dates excluded", grouped(stats.invalid_dates))),
            clock: clock(&stats.hours),
            weekdays,
            donut: donut(&stats.cameras),
            cameras: plain(&stats.cameras, Hue::Violet),
            tags,
            lenses: plain(&stats.lenses, Hue::Teal),
            focal,
            ratings,
            iso,
            aperture,
            shutter,
            cull_survival,
            hit_rate,
            metric,
            rate_cards,
        }
    }
}

/// The hero chart's points: monthly, or yearly when the span passes 180 months, with
/// React's ticks (each January's year; or every `ceil(n / 12)`-th year).
fn timeline(monthly: &[(String, i64)], years: &[(String, i64)]) -> Timeline {
    let yearly = monthly.len() > 180;
    let source = if yearly { years } else { monthly };
    let step = source.len().div_ceil(12).max(1);
    let points = source
        .iter()
        .enumerate()
        .map(|(i, (key, value))| {
            let (tick, readout) = if yearly {
                let tick = if i % step == 0 { key.clone() } else { String::new() };
                (tick, format!("{key} · {} photos", grouped(*value)))
            } else {
                let (year, month) = parse_ym(key);
                let tick = if month == 1 { year.to_string() } else { String::new() };
                (tick, format!("{} · {} photos", fmt_month(key), grouped(*value)))
            };
            TimelinePoint { key: key.clone(), value: *value, tick, readout }
        })
        .collect();
    let peak = source.iter().map(|p| p.1).max().unwrap_or(0).max(1);
    Timeline { yearly, points, peak }
}

/// `RadialClock`'s data; `None` when no photo has a capture hour.
fn clock(hours: &[i64]) -> Option<Clock> {
    if hours.iter().sum::<i64>() == 0 {
        return None;
    }
    let max = hours.iter().copied().max().unwrap_or(0).max(1);
    let peak = hours.iter().position(|&h| h == max).unwrap_or(0);
    let segments = hours
        .iter()
        .enumerate()
        .map(|(hour, &count)| {
            let t = count as f64 / max as f64;
            ClockSegment {
                hour,
                count,
                t,
                color: intensity_color(t),
                title: format!("{}:00–{}:00 · {} photos", hh(hour), hh(hour + 1), grouped(count)),
            }
        })
        .collect();
    Some(Clock { hours: segments, peak: format!("{}–{}", hh(peak), hh(peak + 1)) })
}

/// `CameraDonut`'s data; `None` when no photo names a camera.
fn donut(cameras: &[(String, i64)]) -> Option<Donut> {
    let total: i64 = cameras.iter().map(|c| c.1).sum();
    if total == 0 {
        return None;
    }
    let mut slices: Vec<(String, i64)> = cameras.iter().take(5).cloned().collect();
    let other: i64 = cameras.iter().skip(5).map(|c| c.1).sum();
    if other > 0 {
        slices.push(("Other".into(), other));
    }
    let top_share = format!("{}%", percent(cameras[0].1, total));
    let slices = slices
        .into_iter()
        .enumerate()
        .map(|(i, (name, count))| {
            let pct = percent(count, total);
            DonutSlice {
                title: format!("{name} · {} ({pct}%)", grouped(count)),
                name,
                count,
                pct,
                color: DONUT_HUES[i % DONUT_HUES.len()],
            }
        })
        .collect();
    Some(Donut { slices, top_share })
}

#[cfg(test)]
mod tests;
