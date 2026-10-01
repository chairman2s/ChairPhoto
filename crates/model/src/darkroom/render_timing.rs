//! Frame-timing helpers for the Darkroom's render loop — a port of
//! `src/components/darkroom/renderTiming.ts`. Pure functions only: the stage stamps samples
//! (`crates/app/src/darkroom/stage.rs`), these summarize them.
//!
//! The GPUI stage has no IPC round trip and no `<img>` decode: a frame is *requested* (sent
//! to the image pool), *resolved* (the BGRA frame is back on the UI thread) and *painted*
//! (it is the stage's frame). `ipc` therefore measures pool queue + render + BGRA
//! conversion, and `paint` the hop from the worker's answer to the stage taking it.

use serde::Serialize;
use std::collections::BTreeMap;

use crate::js_compat;

/// Settings key: `"1"` turns the frame log on. Read by the Darkroom once per open and
/// toggled in Preferences → Darkroom.
pub const RENDER_TIMING_KEY: &str = "editor.renderTiming";
/// Settings key the Darkroom writes its latest timing summary under, so a run can be read
/// back without a console.
pub const RENDER_TIMING_SUMMARY_KEY: &str = "editor.renderTiming.lastSummary";

/// Nearest-rank percentile of `xs` (`p` in 0..100). NaN for an empty list; never mutates.
pub fn percentile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut sorted = xs.to_vec();
    // `(a, b) => a - b`: NaN samples never occur (timestamps are finite).
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((js_compat::clamp(p, 0.0, 100.0) / 100.0) * sorted.len() as f64).ceil();
    let i = js_compat::clamp(rank - 1.0, 0.0, sorted.len() as f64 - 1.0) as usize;
    sorted[i]
}

/// p50 and p95 of a list, rounded to 0.1 ms for logging (NaN when empty).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct P50P95 {
    pub p50: f64,
    pub p95: f64,
}

pub fn p50p95(xs: &[f64]) -> P50P95 {
    P50P95 { p50: js_compat::round_tenth(percentile(xs, 50.0)), p95: js_compat::round_tenth(percentile(xs, 95.0)) }
}

/// Which render produced a frame: the 720 px drag tier, the 1400 px settled tier, a
/// geometry base for a GL tier, or a GL draw (the last two only in the React app's probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Fast,
    Settled,
    Base,
    Gl,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Settled => "settled",
            Tier::Base => "base",
            Tier::Gl => "gl",
        }
    }
}

/// One render request's life: asked for, answered, on screen. Times in milliseconds on
/// one clock.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSample {
    pub seq: u64,
    pub tier: Tier,
    pub requested: f64,
    /// When the answer arrived.
    pub resolved: Option<f64>,
    /// When the pixels became the stage's frame.
    pub painted: Option<f64>,
    /// The result arrived but a newer state had already superseded it.
    pub superseded: bool,
}

impl FrameSample {
    pub fn new(seq: u64, tier: Tier, requested: f64) -> Self {
        FrameSample { seq, tier, requested, resolved: None, painted: None, superseded: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimingSummary {
    pub count: usize,
    pub superseded: usize,
    /// request → on screen, every painted frame (the number a drag feels).
    pub latency_ms: P50P95,
    /// request → answer, for frames that were answered.
    pub ipc_ms: P50P95,
    /// answer → on screen for those same frames.
    pub paint_ms: P50P95,
    /// Interval between consecutive painted frames of any tier, within a drag — a gap
    /// longer than [`IDLE_GAP_MS`] is the user pausing, not a slow frame, and is not counted.
    pub cadence_ms: P50P95,
    /// Frames per tier, in tier order (`Partial<Record<Tier, number>>`).
    pub by_tier: BTreeMap<Tier, usize>,
}

impl TimingSummary {
    /// The summary as JSON, the shape the React app persisted (NaN written as `null`).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A pause between painted frames longer than this is idle time, not cadence.
pub const IDLE_GAP_MS: f64 = 1000.0;

pub fn summarize(samples: &[FrameSample]) -> TimingSummary {
    let mut latency = Vec::new();
    let mut ipc = Vec::new();
    let mut paint = Vec::new();
    let mut painted = Vec::new();
    let mut by_tier = BTreeMap::new();
    let mut superseded = 0;
    for s in samples {
        *by_tier.entry(s.tier).or_insert(0) += 1;
        if s.superseded {
            superseded += 1;
        }
        if let Some(r) = s.resolved {
            ipc.push(r - s.requested);
        }
        if let Some(p) = s.painted {
            latency.push(p - s.requested);
            painted.push(p);
            if let Some(r) = s.resolved {
                paint.push(p - r);
            }
        }
    }
    painted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let cadence: Vec<f64> = painted.windows(2).map(|w| w[1] - w[0]).filter(|gap| *gap <= IDLE_GAP_MS).collect();
    TimingSummary {
        count: samples.len(),
        superseded,
        latency_ms: p50p95(&latency),
        ipc_ms: p50p95(&ipc),
        paint_ms: p50p95(&paint),
        cadence_ms: p50p95(&cadence),
        by_tier,
    }
}

/// One log line per painted (or superseded) frame.
pub fn format_sample(s: &FrameSample) -> String {
    let ms = |v: Option<f64>| v.map_or_else(|| "—".to_string(), |v| format!("{}ms", js_compat::to_fixed(v, 1)));
    let total = s.painted.map(|p| p - s.requested);
    let ipc = s.resolved.map(|r| r - s.requested);
    let paint = match (s.resolved, s.painted) {
        (Some(r), Some(p)) => Some(p - r),
        _ => None,
    };
    format!(
        "[edit-timing] seq={} tier={} total={} ipc={} paint={}{}",
        s.seq,
        s.tier.as_str(),
        ms(total),
        ms(ipc),
        ms(paint),
        if s.superseded { " superseded" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/renderTiming.test.ts (11 cases) ---
    use super::*;

    fn sample(seq: u64, tier: Tier, requested: f64, resolved: Option<f64>, painted: Option<f64>, superseded: bool) -> FrameSample {
        FrameSample { seq, tier, requested, resolved, painted, superseded }
    }

    #[test]
    fn percentile_is_nan_for_an_empty_list() {
        assert!(percentile(&[], 50.0).is_nan());
    }

    #[test]
    fn percentile_uses_nearest_rank_on_a_sorted_copy() {
        let xs = [10.0, 1.0, 5.0, 3.0, 8.0, 2.0, 9.0, 4.0, 7.0, 6.0];
        assert_eq!(percentile(&xs, 50.0), 5.0);
        assert_eq!(percentile(&xs, 95.0), 10.0);
        assert_eq!(percentile(&xs, 0.0), 1.0);
        assert_eq!(percentile(&xs, 100.0), 10.0);
    }

    #[test]
    fn percentile_never_mutates_its_input() {
        let xs = vec![3.0, 1.0, 2.0];
        percentile(&xs, 50.0);
        assert_eq!(xs, [3.0, 1.0, 2.0]);
    }

    #[test]
    fn percentile_handles_a_single_sample() {
        assert_eq!(percentile(&[4.2], 95.0), 4.2);
    }

    #[test]
    fn p50p95_rounds_to_a_tenth_of_a_millisecond() {
        assert_eq!(p50p95(&[1.04, 1.06, 1.08]), P50P95 { p50: 1.1, p95: 1.1 });
    }

    #[test]
    fn summarize_splits_ipc_paint_and_cadence_and_counts_superseded_frames() {
        let frames = [
            sample(1, Tier::Fast, 0.0, Some(40.0), Some(50.0), false),
            sample(2, Tier::Fast, 90.0, Some(130.0), None, true),
            sample(3, Tier::Fast, 180.0, Some(220.0), Some(230.0), false),
            sample(3, Tier::Settled, 430.0, Some(530.0), Some(550.0), false),
        ];
        let s = summarize(&frames);
        assert_eq!(s.count, 4);
        assert_eq!(s.superseded, 1);
        assert_eq!(s.latency_ms, P50P95 { p50: 50.0, p95: 120.0 });
        assert_eq!(s.ipc_ms, P50P95 { p50: 40.0, p95: 100.0 });
        assert_eq!(s.paint_ms, P50P95 { p50: 10.0, p95: 20.0 });
        // painted at 50, 230, 550 → intervals 180 and 320
        assert_eq!(s.cadence_ms, P50P95 { p50: 180.0, p95: 320.0 });
        assert_eq!(s.by_tier, BTreeMap::from([(Tier::Fast, 3), (Tier::Settled, 1)]));
    }

    #[test]
    fn summarize_measures_request_to_paint_for_frames_that_never_resolve() {
        let s = summarize(&[
            sample(1, Tier::Fast, 100.0, None, Some(160.0), false),
            sample(2, Tier::Settled, 400.0, None, Some(500.0), false),
        ]);
        assert_eq!(s.latency_ms, P50P95 { p50: 60.0, p95: 100.0 });
        assert!(s.ipc_ms.p50.is_nan());
        assert!(s.paint_ms.p50.is_nan());
        assert_eq!(s.cadence_ms, P50P95 { p50: 340.0, p95: 340.0 });
    }

    #[test]
    fn summarize_does_not_count_a_pause_between_drags_as_cadence() {
        let s = summarize(&[
            sample(1, Tier::Fast, 0.0, None, Some(40.0), false),
            sample(2, Tier::Fast, 90.0, None, Some(130.0), false),
            // …the user lets go, thinks, and drags again 5 s later
            sample(3, Tier::Fast, 5000.0, None, Some(5040.0), false),
            sample(4, Tier::Fast, 5090.0, None, Some(5130.0), false),
        ]);
        assert_eq!(s.cadence_ms, P50P95 { p50: 90.0, p95: 90.0 });
    }

    #[test]
    fn summarize_is_all_nan_but_well_formed_for_no_samples() {
        let s = summarize(&[]);
        assert_eq!(s.count, 0);
        assert!(s.ipc_ms.p50.is_nan());
        assert!(s.cadence_ms.p95.is_nan());
        assert_eq!(
            s.to_json(),
            r#"{"count":0,"superseded":0,"latencyMs":{"p50":null,"p95":null},"ipcMs":{"p50":null,"p95":null},"paintMs":{"p50":null,"p95":null},"cadenceMs":{"p50":null,"p95":null},"byTier":{}}"#
        );
    }

    #[test]
    fn format_sample_prints_the_round_trip_and_paint_and_marks_superseded_frames() {
        assert_eq!(
            format_sample(&sample(7, Tier::Settled, 0.0, Some(12.34), Some(20.0), false)),
            "[edit-timing] seq=7 tier=settled total=20.0ms ipc=12.3ms paint=7.7ms"
        );
        assert_eq!(
            format_sample(&sample(8, Tier::Fast, 0.0, Some(5.0), None, true)),
            "[edit-timing] seq=8 tier=fast total=— ipc=5.0ms paint=— superseded"
        );
        assert_eq!(
            format_sample(&sample(9, Tier::Fast, 10.0, None, Some(52.0), false)),
            "[edit-timing] seq=9 tier=fast total=42.0ms ipc=— paint=—"
        );
    }
}
