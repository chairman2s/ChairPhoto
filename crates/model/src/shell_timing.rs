//! Shell transition timing (dev evidence, behind the Darkroom's render-timing toggle): how
//! long Develop → Library takes and where it goes. Port of `src/modules/shellTiming.ts`.
//!
//! One transition is a record from the Back click to the grid's first render, its first
//! painted thumbnail and the last of the tiles it mounted; the summary is logged as
//! `[shell-timing]` and persisted under [`SHELL_TIMING_KEY`] so it can be read without an
//! inspector. Nothing here runs when the toggle is off beyond a boolean check.
//!
//! Semantic choices against the TypeScript:
//! - **No globals, no clock, no timers.** The TS kept one module-level transition and read
//!   `performance.now()`, `setTimeout` and `requestAnimationFrame` itself. Here the state is
//!   a [`ShellTiming`] value the host owns, every call takes `now` (milliseconds on one
//!   monotonic clock), and the two timers become data: the quiet timeout is a deadline the
//!   host checks with [`ShellTiming::poll`], and the frame-gap detector is
//!   [`ShellTiming::note_frame`], called once per frame. The TS's `microtask` / `timeout0`
//!   marks were scheduled by `markShellLeave`; a host records the equivalent moments with
//!   [`ShellTiming::note_mark`].
//! - **No I/O.** Persisting the setting and logging were side effects of the TS; here
//!   [`ShellTiming::mark_shell_leave`] returns the "started" marker to persist and
//!   [`ShellTiming::poll`] returns the finished [`ShellSummary`] (see
//!   [`ShellSummary::to_json`]) for the host to log and persist.
//! - **Async invokes.** `timedInvoke` wrapped a promise and captured the transition it began
//!   under. Here that is [`ShellTiming::begin_invoke`] → [`InvokeSpan`] →
//!   [`ShellTiming::end_invoke`]; a span from an earlier transition is dropped, which is what
//!   pushing onto a transition nobody reads any more amounted to. [`ShellTiming::timed`]
//!   wraps a synchronous call.
//! - **Rounding** is JavaScript's `Math.round` ([`crate::js_compat`]), ties toward +∞. The
//!   times stay `f64`, so a whole value serializes as `2176.0` where `JSON.stringify` wrote
//!   `2176` — the same JSON number to any reader.
//! - **Marks** keep insertion order, as a JS object with non-index keys does, and serialize
//!   as a JSON object in that order.
//! - Tile counters are `u32`. A tile load reported *before* `start` (a negative bucket) is
//!   counted but not bucketed; the TS wrote it to a `-1` property of the array, which
//!   `JSON.stringify` dropped.

use crate::js_compat::{math_round, round_tenth};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

/// The setting a finished (or started) transition is persisted under.
pub const SHELL_TIMING_KEY: &str = "editor.renderTiming.lastShell";

/// No tile has loaded for this long → the transition is over (lazy tiles below the fold
/// never fire, so "all tiles loaded" is not a usable end condition).
pub const QUIET_MS: f64 = 3000.0;
/// How long a transition that never loads a tile may run before it is finished anyway.
pub const INITIAL_QUIET_MS: f64 = 10000.0;
const BUCKET_MS: f64 = 250.0;
/// Invokes at least this slow during a transition are attributed by name.
const SLOW_INVOKE_MS: f64 = 50.0;

/// Named moments (first occurrence after Back), ms since Back, in the order first hit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks(pub Vec<(String, f64)>);

impl Marks {
    pub fn contains(&self, label: &str) -> bool {
        self.0.iter().any(|(k, _)| k == label)
    }
}

impl Serialize for Marks {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// A backend command that took ≥ 50 ms during the transition.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowInvoke {
    pub cmd: String,
    /// When it started, ms since Back.
    pub start_ms: f64,
    /// Its round trip.
    pub ms: f64,
    /// A cheap size hint the caller attached ([`ShellTiming::note_invoke_rows`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
}

/// One transition as it is being recorded. Times are absolute (the host's clock), except
/// where a field says "ms since Back".
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub from: String,
    pub start: f64,
    /// When the grid's mount render landed, and its own render cost (ms).
    pub commit_at: Option<f64>,
    pub commit_ms: Option<f64>,
    pub first_tile_at: Option<f64>,
    pub tiles_mounted: u32,
    pub tiles_loaded: u32,
    /// When the last tile so far loaded — lazy images below the fold may never load.
    pub last_tile_at: Option<f64>,
    /// Tile loads per 250 ms bucket since Back.
    pub load_buckets: Vec<u32>,
    /// When the grid scrolled to the selection (its mount effect).
    pub scroll_at: Option<f64>,
    /// Longest gap between two frames after Back, and when it ended.
    pub max_frame_gap_ms: f64,
    pub max_frame_gap_at: Option<f64>,
    pub marks: Marks,
    pub slow_invokes: Vec<SlowInvoke>,
    pub finished: bool,
}

/// What a finished transition is logged and persisted as.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSummary {
    pub from: String,
    /// Back → grid render landed.
    pub to_commit_ms: Option<f64>,
    /// The render's own cost.
    pub commit_ms: Option<f64>,
    /// Back → first thumbnail painted.
    pub to_first_tile_ms: Option<f64>,
    /// Back → the last thumbnail that loaded (then nothing for [`QUIET_MS`]).
    pub to_last_tile_ms: Option<f64>,
    pub tiles_mounted: u32,
    pub tiles_loaded: u32,
    /// Tile loads per 250 ms since Back.
    pub load_buckets: Vec<u32>,
    pub to_scroll_ms: Option<f64>,
    pub max_frame_gap_ms: f64,
    pub max_frame_gap_end_ms: Option<f64>,
    pub marks: Marks,
    pub slow_invokes: Vec<SlowInvoke>,
}

impl ShellSummary {
    /// The JSON the TS logged as `[shell-timing] …` and persisted under
    /// [`SHELL_TIMING_KEY`] (camelCase keys, `null` for unknown moments).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a ShellSummary always serializes")
    }
}

/// Reduce a transition to its summary: every moment relative to Back, to a tenth of a ms.
pub fn summarize_transition(t: &Transition) -> ShellSummary {
    let r = |v: Option<f64>| v.map(|v| round_tenth(v - t.start));
    ShellSummary {
        from: t.from.clone(),
        to_commit_ms: r(t.commit_at),
        commit_ms: t.commit_ms.map(round_tenth),
        to_first_tile_ms: r(t.first_tile_at),
        to_last_tile_ms: r(t.last_tile_at),
        tiles_mounted: t.tiles_mounted,
        tiles_loaded: t.tiles_loaded,
        load_buckets: t.load_buckets.clone(),
        to_scroll_ms: r(t.scroll_at),
        max_frame_gap_ms: math_round(t.max_frame_gap_ms),
        max_frame_gap_end_ms: r(t.max_frame_gap_at),
        marks: t.marks.clone(),
        slow_invokes: t.slow_invokes.clone(),
    }
}

/// A backend call in flight, started under the transition current at the time.
#[derive(Debug, Clone, Copy)]
pub struct InvokeSpan {
    transition: u64,
    start: f64,
}

/// The instrument: whether it is on, and the transition being recorded.
#[derive(Debug, Default)]
pub struct ShellTiming {
    enabled: bool,
    current: Option<Transition>,
    /// Bumped by every [`Self::mark_shell_leave`]; ties an [`InvokeSpan`] to its transition.
    generation: u64,
    /// When the quiet timeout fires (the TS's `quiet` timer).
    quiet_deadline: f64,
    /// The previous frame's time (the TS's `requestAnimationFrame` loop's `last`).
    last_frame: f64,
}

impl ShellTiming {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// The transition being recorded, if any (finished ones stay until the next leave).
    pub fn current(&self) -> Option<&Transition> {
        self.current.as_ref()
    }

    fn live(&mut self) -> Option<&mut Transition> {
        self.current.as_mut().filter(|t| !t.finished)
    }

    /// Call at the moment the user leaves a surface (e.g. Back from Develop). Returns the
    /// "started" marker to persist under [`SHELL_TIMING_KEY`], so a transition that never
    /// produces a tile is still visible; `None` when the instrument is off.
    pub fn mark_shell_leave(&mut self, from: &str, now: f64) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.generation += 1;
        self.current = Some(Transition {
            from: from.to_string(),
            start: now,
            commit_at: None,
            commit_ms: None,
            first_tile_at: None,
            tiles_mounted: 0,
            tiles_loaded: 0,
            last_tile_at: None,
            load_buckets: Vec::new(),
            scroll_at: None,
            max_frame_gap_ms: 0.0,
            max_frame_gap_at: None,
            marks: Marks::default(),
            slow_invokes: Vec::new(),
            finished: false,
        });
        self.quiet_deadline = now + INITIAL_QUIET_MS;
        self.last_frame = now;
        Some(serde_json::json!({ "from": from, "started": true }).to_string())
    }

    /// One frame was presented at `now`. The stall detector: a gap far above one frame
    /// budget is the UI thread blocked.
    pub fn note_frame(&mut self, now: f64) {
        let last = self.last_frame;
        let Some(t) = self.live() else { return };
        let gap = now - last;
        if gap > t.max_frame_gap_ms {
            t.max_frame_gap_ms = gap;
            t.max_frame_gap_at = Some(now);
        }
        self.last_frame = now;
    }

    /// Finish the transition if its quiet timeout has passed. Returns the summary to log
    /// (`[shell-timing] {json}`) and persist under [`SHELL_TIMING_KEY`].
    pub fn poll(&mut self, now: f64) -> Option<ShellSummary> {
        let deadline = self.quiet_deadline;
        let t = self.live()?;
        if now < deadline {
            return None;
        }
        t.finished = true;
        Some(summarize_transition(t))
    }

    /// Start timing a backend call. `None` outside a live transition — the call is then a
    /// plain passthrough.
    pub fn begin_invoke(&mut self, now: f64) -> Option<InvokeSpan> {
        let generation = self.generation;
        self.live()?;
        Some(InvokeSpan { transition: generation, start: now })
    }

    /// Finish timing a backend call: a slow one is attributed by name to the transition it
    /// started under (dropped if that transition has since been replaced).
    pub fn end_invoke(&mut self, span: InvokeSpan, cmd: &str, now: f64) {
        if span.transition != self.generation {
            return;
        }
        let Some(t) = self.current.as_mut() else { return };
        let ms = now - span.start;
        if ms >= SLOW_INVOKE_MS {
            t.slow_invokes.push(SlowInvoke {
                cmd: cmd.to_string(),
                start_ms: round_tenth(span.start - t.start),
                ms: round_tenth(ms),
                rows: None,
            });
        }
    }

    /// Wrap a synchronous backend call; `now` reads the host clock.
    pub fn timed<T>(&mut self, cmd: &str, mut now: impl FnMut() -> f64, run: impl FnOnce() -> T) -> T {
        let span = self.begin_invoke(now());
        let out = run();
        if let Some(span) = span {
            self.end_invoke(span, cmd, now());
        }
        out
    }

    /// Attach a row count to the most recent slow invoke of `cmd`.
    pub fn note_invoke_rows(&mut self, cmd: &str, rows: usize) {
        let Some(t) = self.current.as_mut() else { return };
        if let Some(inv) = t.slow_invokes.iter_mut().rev().find(|i| i.cmd == cmd) {
            inv.rows = Some(rows);
        }
    }

    /// Record a named moment once per transition (the first time it is hit).
    pub fn note_mark(&mut self, label: &str, now: f64) {
        let Some(t) = self.live() else { return };
        if t.marks.contains(label) {
            return;
        }
        let at = round_tenth(now - t.start);
        t.marks.0.push((label.to_string(), at));
    }

    /// The grid scrolled to the selected photo on mount.
    pub fn note_grid_scroll(&mut self, now: f64) {
        let Some(t) = self.live() else { return };
        if t.scroll_at.is_none() {
            t.scroll_at = Some(now);
        }
    }

    /// The grid's mount render landed; `actual_duration` is its own cost in ms. Any other
    /// `phase` (an update render) is ignored.
    pub fn note_grid_commit(&mut self, phase: &str, actual_duration: f64, now: f64) {
        if phase != "mount" {
            return;
        }
        let Some(t) = self.live() else { return };
        t.commit_at = Some(now);
        t.commit_ms = Some(actual_duration);
    }

    /// A thumbnail tile mounted (whether or not its image has loaded yet).
    pub fn note_tile_mounted(&mut self) {
        if let Some(t) = self.live() {
            t.tiles_mounted += 1;
        }
    }

    /// A thumbnail's image finished loading — the tile is painted on the next frame. Pushes
    /// the quiet deadline out by [`QUIET_MS`].
    pub fn note_tile_loaded(&mut self, now: f64) {
        let Some(t) = self.live() else { return };
        t.tiles_loaded += 1;
        if t.first_tile_at.is_none() {
            t.first_tile_at = Some(now);
        }
        t.last_tile_at = Some(now);
        let b = ((now - t.start) / BUCKET_MS).floor();
        if b >= 0.0 {
            let b = b as usize;
            if t.load_buckets.len() <= b {
                t.load_buckets.resize(b + 1, 0);
            }
            t.load_buckets[b] += 1;
        }
        self.quiet_deadline = now + QUIET_MS;
    }
}

// Port of `src/modules/__tests__/shellTiming.test.ts` (3 cases, same names): pure summary
// math, and the no-op contract when no transition is in flight.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_every_moment_relative_to_back_to_a_tenth_of_a_millisecond() {
        let summary = summarize_transition(&Transition {
            from: "develop".into(),
            start: 1000.0,
            commit_at: Some(1118.44),
            commit_ms: Some(25.06),
            first_tile_at: Some(1198.0),
            last_tile_at: Some(3585.0),
            tiles_mounted: 310,
            tiles_loaded: 66,
            load_buckets: vec![56, 0, 0, 10],
            scroll_at: Some(1123.0),
            max_frame_gap_ms: 2176.4,
            max_frame_gap_at: Some(3317.0),
            marks: Marks(vec![("grid-effects".into(), 120.0)]),
            slow_invokes: vec![SlowInvoke {
                cmd: "list_tags".into(),
                start_ms: 120.0,
                ms: 2240.0,
                rows: Some(1614),
            }],
            finished: true,
        });
        assert_eq!(
            summary,
            ShellSummary {
                from: "develop".into(),
                to_commit_ms: Some(118.4),
                commit_ms: Some(25.1),
                to_first_tile_ms: Some(198.0),
                to_last_tile_ms: Some(2585.0),
                tiles_mounted: 310,
                tiles_loaded: 66,
                load_buckets: vec![56, 0, 0, 10],
                to_scroll_ms: Some(123.0),
                max_frame_gap_ms: 2176.0,
                max_frame_gap_end_ms: Some(2317.0),
                marks: Marks(vec![("grid-effects".into(), 120.0)]),
                slow_invokes: vec![SlowInvoke {
                    cmd: "list_tags".into(),
                    start_ms: 120.0,
                    ms: 2240.0,
                    rows: Some(1614),
                }],
            }
        );
    }

    #[test]
    fn leaves_unknown_moments_null_rather_than_inventing_zeros() {
        let s = summarize_transition(&Transition {
            from: "develop".into(),
            start: 0.0,
            commit_at: None,
            commit_ms: None,
            first_tile_at: None,
            tiles_mounted: 0,
            tiles_loaded: 0,
            last_tile_at: None,
            load_buckets: vec![],
            scroll_at: None,
            max_frame_gap_ms: 0.0,
            max_frame_gap_at: None,
            marks: Marks::default(),
            slow_invokes: vec![],
            finished: true,
        });
        assert_eq!(s.to_commit_ms, None);
        assert_eq!(s.to_first_tile_ms, None);
        assert_eq!(s.max_frame_gap_end_ms, None);
    }

    #[test]
    fn timed_invoke_is_a_passthrough_and_the_note_functions_are_no_ops() {
        let mut timing = ShellTiming::new();
        assert_eq!(timing.timed("x", || 0.0, || 42), 42);
        timing.note_mark("anything", 1.0);
        timing.note_tile_loaded(1.0);
        assert!(timing.current().is_none());
    }

    // New (not in the vitest file): the timers the TS kept in `setTimeout` /
    // `requestAnimationFrame` are data here, so their contract is testable directly.
    #[test]
    fn a_transition_finishes_quiet_ms_after_its_last_tile() {
        let mut timing = ShellTiming::new();
        assert_eq!(timing.mark_shell_leave("develop", 1000.0), None, "off by default");
        timing.set_enabled(true);
        let started = timing.mark_shell_leave("develop", 1000.0).unwrap();
        assert_eq!(started, r#"{"from":"develop","started":true}"#);

        timing.note_frame(1016.0);
        timing.note_frame(1300.0); // a 284 ms stall
        timing.note_frame(1316.0);
        timing.note_tile_mounted();
        timing.note_tile_loaded(1400.0);
        timing.note_mark("grid-effects", 1120.04);
        timing.note_mark("grid-effects", 1500.0); // first occurrence only

        let span = timing.begin_invoke(1100.0).unwrap();
        timing.end_invoke(span, "list_tags", 1180.0);
        timing.note_invoke_rows("list_tags", 7);

        assert_eq!(timing.poll(4399.0), None, "still inside QUIET_MS of the last tile");
        let s = timing.poll(4400.0).unwrap();
        assert_eq!(s.to_first_tile_ms, Some(400.0));
        assert_eq!(s.load_buckets, vec![0, 1]);
        assert_eq!(s.max_frame_gap_ms, 284.0);
        assert_eq!(s.max_frame_gap_end_ms, Some(300.0));
        assert_eq!(s.marks, Marks(vec![("grid-effects".into(), 120.0)]));
        assert_eq!(
            s.slow_invokes,
            vec![SlowInvoke { cmd: "list_tags".into(), start_ms: 100.0, ms: 80.0, rows: Some(7) }]
        );
        // Finished: nothing more is recorded, and it does not finish twice.
        timing.note_tile_loaded(4500.0);
        assert_eq!(timing.current().unwrap().tiles_loaded, 1);
        assert_eq!(timing.poll(99999.0), None);
    }

    #[test]
    fn an_invoke_from_a_replaced_transition_is_not_attributed() {
        let mut timing = ShellTiming::new();
        timing.set_enabled(true);
        timing.mark_shell_leave("develop", 0.0);
        let span = timing.begin_invoke(10.0).unwrap();
        timing.mark_shell_leave("develop", 20.0);
        timing.end_invoke(span, "list_photos", 500.0);
        assert!(timing.current().unwrap().slow_invokes.is_empty());
        // A summary's JSON omits an absent row hint and keeps unknown moments as null.
        let json = timing.poll(10020.0).unwrap().to_json();
        assert!(json.contains(r#""toCommitMs":null"#), "{json}");
        assert!(!json.contains("rows"), "{json}");
    }
}
