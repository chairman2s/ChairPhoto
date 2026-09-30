//! The Darkroom's autosave history (docs/editing.md § History) — a port of
//! `src/components/darkroom/history.ts`, plus `whenLabel` from `HistoryPanel.tsx`. Pure
//! helpers that name a change and decide whether it continues the previous step; the steps
//! themselves live in the catalog (`photo_version_history`).
//!
//! Labels follow JS formatting exactly: `toFixed` rounds ties up and `Math.round` rounds
//! halves toward +∞ ([`crate::js_compat`]), and the minus sign is U+2212 "−".

use serde::Serialize;

use crate::editing::{Tone, VersionEdit, Wb};
use crate::js_compat::{self, number_to_string, to_fixed};

/// How long the same control may keep moving and still amend the step it started.
pub const AMEND_WINDOW_MS: i64 = 4000;

/// A change between two records: a label for the History panel, and a key naming the
/// control(s) that changed — equal keys can coalesce into one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub label: String,
    pub key: String,
}

type ToneGet = fn(&Tone) -> Option<f64>;

const TONE_NAMES: [(&str, &str, ToneGet); 8] = [
    ("ev", "Exposure", |t| t.ev),
    ("contrast", "Contrast", |t| t.contrast),
    ("highlights", "Highlights", |t| t.highlights),
    ("shadows", "Shadows", |t| t.shadows),
    ("whites", "Whites", |t| t.whites),
    ("blacks", "Blacks", |t| t.blacks),
    ("vibrance", "Vibrance", |t| t.vibrance),
    ("saturation", "Saturation", |t| t.saturation),
];

/// `+0.50` / `−0.25` (U+2212), two decimals.
fn signed(v: f64) -> String {
    format!("{}{}", if v >= 0.0 { "+" } else { "−" }, to_fixed(v.abs(), 2))
}

/// TS `same`: `JSON.stringify(a ?? null) === JSON.stringify(b ?? null)`. Compared as JSON
/// values, so NaN equals NaN (both `null`) as in TS; unlike TS the comparison ignores key
/// order, which in TS followed how each object was built — never meaningfully different.
fn same<T: Serialize>(a: &Option<T>, b: &Option<T>) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// Strip one trailing `.cube`, any case (`/\.cube$/i`).
fn strip_cube(file: &str) -> &str {
    let n = file.len();
    if n >= 5 && file.is_char_boundary(n - 5) && file[n - 5..].eq_ignore_ascii_case(".cube") {
        &file[..n - 5]
    } else {
        file
    }
}

/// Every control that differs between `prev` and `next`, as (key, label) pairs, in panel
/// order.
fn changed_controls(prev: &VersionEdit, next: &VersionEdit) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, l: String| out.push((k.to_string(), l));
    let pt = prev.tone.clone().unwrap_or_default();
    let nt = next.tone.clone().unwrap_or_default();
    for (k, name, get) in TONE_NAMES {
        let a = get(&pt).unwrap_or(0.0);
        let b = get(&nt).unwrap_or(0.0);
        if a != b {
            push(&format!("tone.{k}"), format!("{name} {}", signed(b)));
        }
    }
    let pwb = pt.wb.clone().unwrap_or_else(|| Wb::relative(0.0, 0.0));
    let nwb = nt.wb.clone().unwrap_or_else(|| Wb::relative(0.0, 0.0));
    let pk = if pwb.is_kelvin() { pwb.kelvin } else { None };
    let nk = if nwb.is_kelvin() { nwb.kelvin } else { None };
    if pk != nk {
        // Kelvin (slice 9): the stated light, or back to as-shot.
        let label = match nk {
            Some(k) => format!("White balance {} K", number_to_string(js_compat::round(k))),
            None => "White balance as shot".to_string(),
        };
        push("tone.wb.kelvin", label);
    } else if nk.is_some() {
        let t = nwb.tint.unwrap_or(0.0);
        if pwb.tint.unwrap_or(0.0) != t {
            let sign = if t >= 0.0 { "+" } else { "−" };
            push("tone.wb.tint", format!("Tint {sign}{}", number_to_string(js_compat::round(t).abs())));
        }
    }
    if nk.is_none() && pk.is_none() {
        if pwb.temp.unwrap_or(0.0) != nwb.temp.unwrap_or(0.0) {
            push("tone.wb.temp", format!("Temperature {}", signed(nwb.temp.unwrap_or(0.0))));
        }
        if pwb.tint.unwrap_or(0.0) != nwb.tint.unwrap_or(0.0) {
            push("tone.wb.tint", format!("Tint {}", signed(nwb.tint.unwrap_or(0.0))));
        }
    }
    if !same(&prev.zones, &next.zones) {
        push("zones", "Tone strip".into());
    }
    if !same(&prev.crop, &next.crop) {
        let label = match &next.crop {
            Some(c) => match c.aspect.as_deref() {
                Some(a) if !a.is_empty() && a != "Free" => format!("Crop {a}"),
                _ => "Crop".into(),
            },
            None => "Crop removed".into(),
        };
        push("crop", label);
    }
    if prev.straighten.unwrap_or(0.0) != next.straighten.unwrap_or(0.0) {
        push("straighten", format!("Straighten {}°", to_fixed(next.straighten.unwrap_or(0.0), 1)));
    }
    if !same(&prev.perspective, &next.perspective) {
        push("perspective", if next.perspective.is_some() { "Perspective" } else { "Perspective removed" }.into());
    }
    let lens_on = |e: &VersionEdit| e.lens.as_ref().is_some_and(|l| l.builtin);
    if lens_on(prev) != lens_on(next) {
        push("lens", if lens_on(next) { "Lens correction on" } else { "Lens correction off" }.into());
    }
    if !same(&prev.bw, &next.bw) {
        push("bw", if next.bw.as_ref().is_some_and(|b| b.enabled) { "Black & white" } else { "Colour" }.into());
    }
    if !same(&prev.split, &next.split) {
        push("split", "Split toning".into());
    }
    if !same(&prev.grain, &next.grain) {
        push("grain", "Grain".into());
    }
    if prev.fade.unwrap_or(0.0) != next.fade.unwrap_or(0.0) {
        push("fade", format!("Fade {}", to_fixed(next.fade.unwrap_or(0.0), 2)));
    }
    if prev.vignette.unwrap_or(0.0) != next.vignette.unwrap_or(0.0) {
        push("vignette", format!("Vignette {}", signed(next.vignette.unwrap_or(0.0))));
    }
    if !same(&prev.lut, &next.lut) {
        let label = match &next.lut {
            Some(l) => format!("LUT {}", strip_cube(&l.file)),
            None => "LUT removed".into(),
        };
        push("lut", label);
    }
    out
}

/// Name the change from `prev` to `next`. One control: its own label ("Exposure +0.50").
/// Several at once (a preset, a proof, a reset): `several` if given, else "Adjustments".
///
/// As in TS, `several` is tested for *truthiness* where it decides single vs. multi (an
/// empty string there does not count as a caller label) and for presence where it is the
/// label (`several ?? "Edit"` keeps an empty string).
pub fn describe_change(prev: &VersionEdit, next: &VersionEdit, several: Option<&str>) -> Change {
    let changed = changed_controls(prev, next);
    if changed.is_empty() {
        return Change { label: several.unwrap_or("Edit").into(), key: "none".into() };
    }
    let has_label = several.is_some_and(|s| !s.is_empty());
    if changed.len() == 1 && !has_label {
        let (key, label) = changed.into_iter().next().expect("one change");
        return Change { label, key };
    }
    let keys: Vec<&str> = changed.iter().map(|(k, _)| k.as_str()).collect();
    Change { label: several.unwrap_or("Adjustments").into(), key: format!("multi:{}", keys.join(",")) }
}

/// The newest step, as [`should_amend`] needs it: its change key and when it was made (ms).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastStep {
    pub key: String,
    pub at: i64,
}

/// Whether a new step should amend the previous one: the same single control, still moving
/// (within the window), and the previous step is the newest one. A multi-control change
/// always stands as its own step.
pub fn should_amend(last: Option<&LastStep>, change: &Change, now: i64, at_tip: bool) -> bool {
    let Some(last) = last else { return false };
    at_tip
        && change.key == last.key
        && !change.key.starts_with("multi:")
        && change.key != "none"
        && now - last.at < AMEND_WINDOW_MS
}

/// When a history step was made, relative to now (`whenLabel`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WhenLabel {
    /// Under 45 s ago, or in the future.
    JustNow,
    MinutesAgo(f64),
    HoursAgo(f64),
    /// A day or more ago: the step's time in seconds since the epoch. TS printed
    /// `toLocaleDateString()`, which depends on the locale, so the UI formats it.
    Date(f64),
}

impl WhenLabel {
    /// The label; `date` formats [`WhenLabel::Date`]'s seconds in the user's locale.
    pub fn text(self, date: impl FnOnce(f64) -> String) -> String {
        match self {
            WhenLabel::JustNow => "just now".into(),
            WhenLabel::MinutesAgo(m) => format!("{} min ago", number_to_string(m)),
            WhenLabel::HoursAgo(h) => format!("{} h ago", number_to_string(h)),
            WhenLabel::Date(secs) => date(secs),
        }
    }
}

/// How long ago a step made at `created_at_secs` was, at `now_ms`.
pub fn when_label(created_at_secs: f64, now_ms: f64) -> WhenLabel {
    let s = js_compat::max(0.0, js_compat::round(now_ms / 1000.0 - created_at_secs));
    if s < 45.0 {
        WhenLabel::JustNow
    } else if s < 3600.0 {
        WhenLabel::MinutesAgo(js_compat::round(s / 60.0))
    } else if s < 86400.0 {
        WhenLabel::HoursAgo(js_compat::round(s / 3600.0))
    } else {
        WhenLabel::Date(created_at_secs)
    }
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/history.test.ts (8 cases) ---
    use super::*;
    use crate::editing::parse_edit;
    use serde_json::{json, Value};

    fn rec(v: Value) -> VersionEdit {
        parse_edit(Some(&v.to_string()))
    }

    fn label(prev: Value, next: Value) -> String {
        describe_change(&rec(prev), &rec(next), None).label
    }

    #[test]
    fn names_a_single_slider_with_its_new_value() {
        assert_eq!(
            describe_change(&rec(json!({})), &rec(json!({"tone": {"ev": 0.5}})), None),
            Change { label: "Exposure +0.50".into(), key: "tone.ev".into() }
        );
        assert_eq!(label(json!({"tone": {"ev": 0.5}}), json!({"tone": {"ev": -0.25}})), "Exposure −0.25");
        assert_eq!(label(json!({}), json!({"tone": {"wb": {"temp": 0.1, "tint": 0}}})), "Temperature +0.10");
    }

    #[test]
    fn names_geometry_and_looks() {
        assert_eq!(label(json!({}), json!({"crop": {"x": 0, "y": 0, "w": 1, "h": 1, "aspect": "4:5"}})), "Crop 4:5");
        assert_eq!(label(json!({"crop": {"x": 0, "y": 0, "w": 1, "h": 1}}), json!({})), "Crop removed");
        assert_eq!(label(json!({}), json!({"straighten": 1.46})), "Straighten 1.5°");
        assert_eq!(label(json!({}), json!({"bw": {"enabled": true, "r": 1, "g": 0, "b": 0}})), "Black & white");
        assert_eq!(label(json!({}), json!({"lut": {"file": "portra.cube", "amount": 1}})), "LUT portra");
        assert_eq!(label(json!({}), json!({"zones": [0, 0.2, 0, 0, 0, 0, 0, 0]})), "Tone strip");
        assert_eq!(
            describe_change(&rec(json!({})), &rec(json!({"lens": {"builtin": true}})), None),
            Change { label: "Lens correction on".into(), key: "lens".into() }
        );
        assert_eq!(label(json!({"lens": {"builtin": true}}), json!({})), "Lens correction off");
    }

    #[test]
    fn names_several_changes_at_once_by_the_callers_label_or_generically() {
        let from = rec(json!({}));
        let to = rec(json!({"tone": {"ev": 0.3, "contrast": 0.2}, "fade": 0.1}));
        assert_eq!(describe_change(&from, &to, Some("Proof: Portra")).label, "Proof: Portra");
        let generic = describe_change(&from, &to, None);
        assert_eq!(generic.label, "Adjustments");
        assert!(generic.key.starts_with("multi:"));
        // A caller label wins even for a single control (a reset of one slider, say).
        assert_eq!(describe_change(&rec(json!({"fade": 0.2})), &rec(json!({})), Some("Reset")).label, "Reset");
    }

    #[test]
    fn says_edit_when_nothing_changed() {
        assert_eq!(describe_change(&rec(json!({"fade": 0.2})), &rec(json!({"fade": 0.2})), None).key, "none");
    }

    fn exposure() -> Change {
        Change { label: "Exposure +0.50".into(), key: "tone.ev".into() }
    }

    fn last(key: &str, at: i64) -> LastStep {
        LastStep { key: key.into(), at }
    }

    #[test]
    fn amends_the_same_control_still_moving_at_the_tip() {
        assert!(should_amend(Some(&last("tone.ev", 1000)), &exposure(), 1000 + AMEND_WINDOW_MS - 1, true));
    }

    #[test]
    fn starts_a_new_step_otherwise() {
        assert!(!should_amend(None, &exposure(), 0, true));
        assert!(!should_amend(Some(&last("tone.ev", 0)), &exposure(), AMEND_WINDOW_MS, true));
        assert!(!should_amend(Some(&last("tone.contrast", 0)), &exposure(), 10, true));
        assert!(!should_amend(Some(&last("tone.ev", 0)), &exposure(), 10, false));
        let multi = Change { label: "x".into(), key: "multi:a,b".into() };
        assert!(!should_amend(Some(&last("multi:a,b", 0)), &multi, 10, true));
    }

    const NOW: f64 = 1_800_000_000_000.0; // ms

    fn at(secs_ago: f64) -> f64 {
        NOW / 1000.0 - secs_ago
    }

    #[test]
    fn when_label_reads_naturally_for_recent_steps() {
        let text = |w: WhenLabel| w.text(|_| unreachable!());
        assert_eq!(text(when_label(at(5.0), NOW)), "just now");
        assert_eq!(text(when_label(at(5.0 * 60.0), NOW)), "5 min ago");
        assert_eq!(text(when_label(at(3.0 * 3600.0), NOW)), "3 h ago");
    }

    #[test]
    fn when_label_falls_back_to_the_date_after_a_day_and_never_goes_negative() {
        // TS compared against `toLocaleDateString()`; the locale formatting is the UI's.
        assert_eq!(when_label(at(3.0 * 86400.0), NOW), WhenLabel::Date(NOW / 1000.0 - 3.0 * 86400.0));
        assert_eq!(when_label(at(3.0 * 86400.0), NOW).text(|s| format!("day {s}")), format!("day {}", NOW / 1000.0 - 3.0 * 86400.0));
        assert_eq!(when_label(at(-60.0), NOW), WhenLabel::JustNow);
    }

    // --- JS-formatting edge cases (new) ---

    #[test]
    fn labels_follow_js_number_formatting() {
        // toFixed rounds an exact tie up (Rust's {:.2} would print 0.12).
        assert_eq!(label(json!({}), json!({"fade": 0.125})), "Fade 0.13");
        // A negative straighten keeps its sign; Math.round(-2.5) is -2.
        assert_eq!(label(json!({}), json!({"straighten": -1.25})), "Straighten -1.3°");
        let k = |kelvin: f64, tint: f64| json!({"tone": {"wb": {"temp": 0, "tint": tint, "mode": "kelvin", "kelvin": kelvin}}});
        assert_eq!(label(k(4800.0, 0.0), k(4800.0, -2.5)), "Tint −2");
        assert_eq!(label(json!({}), k(4812.5, 0.0)), "White balance 4813 K");
        assert_eq!(label(json!({}), json!({"lut": {"file": "Look.CUBE", "amount": 1}})), "LUT Look");
        // An empty caller label does not force a multi step, but is still the "nothing" label.
        assert_eq!(describe_change(&rec(json!({})), &rec(json!({"fade": 0.2})), Some("")).key, "fade");
        assert_eq!(describe_change(&rec(json!({})), &rec(json!({})), Some("")).label, "");
    }
}
