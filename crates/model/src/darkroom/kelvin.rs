//! Kelvin white balance in the Darkroom — a port of `src/components/darkroom/kelvin.ts`
//! (docs/plans/raw-foundation, slice 9). A Kelvin record states the scene's light —
//! `tone.wb = { mode: "kelvin", kelvin, tint }` — and renders on engine 2 around the photo's
//! as-shot light. Rendering the as-shot Kelvin and tint is the identity, so "as shot" and a
//! blank white balance are the same picture.

use crate::editing::{Field, Tone, VersionEdit, Wb};
use crate::js_compat;
use serde_json::Map;

/// Settings key: which white-balance slider a fresh engine-2 edit shows — `"kelvin"`
/// (default) or `"relative"`.
pub const WB_SLIDER_KEY: &str = "develop.wbSlider";

/// How far the proof sheet's warm/cool cells move the light, in mireds.
pub const PROOF_WARMTH_MIREDS: f64 = 30.0;
/// The duel's first warmth step, in mireds (halved on every revisit).
pub const DUEL_WARMTH_MIREDS: f64 = 25.0;

pub const KELVIN_MIN: f64 = 2000.0;
pub const KELVIN_MAX: f64 = 12000.0;
/// Tint in Kelvin mode: +100 is one stop less green. The slider spans ±50.
pub const KELVIN_TINT_RANGE: f64 = 50.0;
/// The slider's resolution: positions 0..SLIDER_STEPS map KELVIN_MIN..KELVIN_MAX.
pub const SLIDER_STEPS: f64 = 1000.0;

/// The photo's as-shot light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AsShotWb {
    pub kelvin: f64,
    pub tint: f64,
}

/// Which white-balance slider a fresh edit shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WbPrefer {
    Kelvin,
    Relative,
}

impl WbPrefer {
    /// The [`WB_SLIDER_KEY`] setting's value: `"relative"` picks relative, anything else
    /// (including unset) the default Kelvin.
    pub fn from_setting(v: Option<&str>) -> Self {
        if v == Some("relative") {
            WbPrefer::Relative
        } else {
            WbPrefer::Kelvin
        }
    }
}

/// What Kelvin needs of the photo, and which slider a fresh edit shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KelvinContext {
    pub as_shot: AsShotWb,
    pub prefer: WbPrefer,
}

/// Slider position for a Kelvin value — logarithmic, so equal travel is an equal ratio of
/// temperature. An integer (JS `Math.round`).
pub fn kelvin_to_slider(kelvin: f64) -> f64 {
    let k = js_compat::clamp(kelvin, KELVIN_MIN, KELVIN_MAX);
    js_compat::round(SLIDER_STEPS * (k / KELVIN_MIN).ln() / (KELVIN_MAX / KELVIN_MIN).ln())
}

/// Kelvin for a slider position, rounded to 50 K.
pub fn slider_to_kelvin(pos: f64) -> f64 {
    let p = js_compat::clamp(pos, 0.0, SLIDER_STEPS);
    let k = KELVIN_MIN * (KELVIN_MAX / KELVIN_MIN).powf(p / SLIDER_STEPS);
    js_compat::round(k / 50.0) * 50.0
}

/// `kelvin` moved by `mireds` (negative = a higher Kelvin = a warmer picture), in range.
/// Clamped in mireds before inverting: a shift past 0 mireds lands on the warm end.
pub fn mired_shift(kelvin: f64, mireds: f64) -> f64 {
    let m = js_compat::clamp(1e6 / kelvin + mireds, 1e6 / KELVIN_MAX, 1e6 / KELVIN_MIN);
    js_compat::round(1e6 / m)
}

/// A Kelvin white balance for the record: `{ temp: 0, tint, mode: "kelvin", kelvin }`.
pub fn kelvin_wb(kelvin: f64, tint: f64) -> Wb {
    Wb {
        temp: Field::Set(0.0),
        tint: Field::Set(tint),
        mode: Field::Set("kelvin".into()),
        kelvin: Field::Set(kelvin),
        extra: Map::new(),
    }
}

/// What the white-balance rail shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WbShown {
    Kelvin { kelvin: f64, tint: f64 },
    Relative,
}

/// What the rail shows for `wb`: the record's Kelvin pair; as-shot Kelvin when the record
/// leaves white balance alone and Kelvin is preferred; otherwise the relative sliders.
/// Kelvin needs a context — engine 2 with an as-shot light. Pass `None` for a `null` white
/// balance, and for a raw (non-object) one: TS's `!wb` or its missing members made it
/// untouched.
pub fn wb_shown(wb: Option<&Wb>, ctx: Option<&KelvinContext>) -> WbShown {
    if let Some(w) = wb {
        // `wb.kelvin != null`: a raw one counts, read as a number.
        if w.is_kelvin() && !w.kelvin.is_nullish() {
            return WbShown::Kelvin { kelvin: w.kelvin.num_or(0.0), tint: w.tint.num_or(0.0) };
        }
    }
    let Some(ctx) = ctx else { return WbShown::Relative };
    let untouched = match wb {
        None => true,
        Some(w) => w.mode.as_deref() != Some("relative") && w.temp.strictly_equals_or(0.0, 0.0) && w.tint.strictly_equals_or(0.0, 0.0),
    };
    if ctx.prefer == WbPrefer::Kelvin && untouched {
        return WbShown::Kelvin { kelvin: ctx.as_shot.kelvin, tint: ctx.as_shot.tint };
    }
    WbShown::Relative
}

/// `record` with its white balance moved `mireds` warmer (negative) or cooler, in Kelvin
/// around what it shows now — the proof sheet's warm/cool cells and the duel's warmth.
pub fn with_kelvin_shift(record: &VersionEdit, ctx: &KelvinContext, mireds: f64) -> VersionEdit {
    let prefer_kelvin = KelvinContext { prefer: WbPrefer::Kelvin, ..*ctx };
    let (kelvin, tint) = match wb_shown(record.tone.value().and_then(|t| t.wb.value()), Some(&prefer_kelvin)) {
        WbShown::Kelvin { kelvin, tint } => (kelvin, tint),
        WbShown::Relative => (ctx.as_shot.kelvin, ctx.as_shot.tint),
    };
    let tone = Tone { wb: Field::Set(kelvin_wb(mired_shift(kelvin, mireds), tint)), ..record.tone.value_or_default() };
    VersionEdit { tone: Field::Set(tone), ..record.clone() }
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/kelvin.test.ts (9 cases) ---
    use super::*;
    use crate::darkroom::history::describe_change;
    use crate::darkroom::spreads::{duel_pair, proof_spread, DuelDim};
    use crate::editing::parse_edit;
    use serde_json::json;

    const CTX: KelvinContext = KelvinContext { as_shot: AsShotWb { kelvin: 5313.0, tint: 2.4 }, prefer: WbPrefer::Kelvin };

    fn wb_of(r: &VersionEdit) -> Wb {
        r.tone.value().and_then(|t| t.wb.value().cloned()).expect("a white balance")
    }

    #[test]
    fn the_slider_spans_the_range_logarithmically_and_round_trips_to_50_k() {
        assert_eq!(kelvin_to_slider(KELVIN_MIN), 0.0);
        assert_eq!(kelvin_to_slider(KELVIN_MAX), SLIDER_STEPS);
        for k in [2500.0, 3200.0, 5200.0, 6500.0, 9000.0] {
            assert!((slider_to_kelvin(kelvin_to_slider(k)) - k).abs() <= 50.0, "{k}");
        }
        // Log: the midpoint is the geometric mean, not the arithmetic one.
        assert_eq!(slider_to_kelvin(SLIDER_STEPS / 2.0), js_compat::round((KELVIN_MIN * KELVIN_MAX).sqrt() / 50.0) * 50.0);
    }

    #[test]
    fn a_negative_mired_shift_is_a_higher_kelvin_clamped_to_range() {
        assert!(mired_shift(5000.0, -30.0) > 5000.0);
        assert!(mired_shift(5000.0, 30.0) < 5000.0);
        assert_eq!(mired_shift(11800.0, -100.0), KELVIN_MAX);
    }

    #[test]
    fn a_kelvin_record_shows_its_own_light() {
        assert_eq!(wb_shown(Some(&kelvin_wb(4200.0, -3.0)), Some(&CTX)), WbShown::Kelvin { kelvin: 4200.0, tint: -3.0 });
        assert_eq!(wb_shown(Some(&kelvin_wb(4200.0, -3.0)), None), WbShown::Kelvin { kelvin: 4200.0, tint: -3.0 });
    }

    #[test]
    fn an_untouched_white_balance_shows_as_shot_kelvin_when_preferred_relative_otherwise() {
        let zero = Wb::relative(0.0, 0.0);
        assert_eq!(wb_shown(Some(&zero), Some(&CTX)), WbShown::Kelvin { kelvin: 5313.0, tint: 2.4 });
        assert_eq!(wb_shown(None, Some(&CTX)), WbShown::Kelvin { kelvin: 5313.0, tint: 2.4 });
        let relative = KelvinContext { prefer: WbPrefer::Relative, ..CTX };
        assert_eq!(wb_shown(Some(&zero), Some(&relative)), WbShown::Relative);
        assert_eq!(wb_shown(Some(&zero), None), WbShown::Relative);
    }

    #[test]
    fn a_relative_edit_or_an_explicit_relative_choice_stays_relative() {
        assert_eq!(wb_shown(Some(&Wb::relative(0.3, 0.0)), Some(&CTX)), WbShown::Relative);
        let explicit = Wb { mode: Field::Set("relative".into()), ..Wb::relative(0.0, 0.0) };
        assert_eq!(wb_shown(Some(&explicit), Some(&CTX)), WbShown::Relative);
    }

    #[test]
    fn the_proof_sheets_warm_cell_states_a_higher_kelvin_than_as_shot_the_cool_one_lower() {
        let cells = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], Some(&CTX));
        let warm = wb_of(&cells.iter().find(|c| c.label == "Auto · Warm").unwrap().record);
        let cool = wb_of(&cells.iter().find(|c| c.label == "Auto · Cool").unwrap().record);
        assert_eq!(warm.mode.as_deref(), Some("kelvin"));
        assert!(warm.kelvin.get().unwrap() > 5313.0);
        assert!(cool.kelvin.get().unwrap() < 5313.0);
        assert!((warm.tint.get().unwrap() - 2.4).abs() < 0.005);
        // Without the context (engine 1, or no as-shot light) it stays the relative nudge.
        let rel = proof_spread(&VersionEdit::default(), &VersionEdit::default(), &[], None);
        let rel = wb_of(&rel.iter().find(|c| c.label == "Auto · Warm").unwrap().record);
        assert_eq!(rel.mode, Field::Absent);
        assert!(rel.temp.get().unwrap() > 0.0);
    }

    #[test]
    fn the_duels_warmth_round_steps_stated_light_around_what_the_photo_shows() {
        let working = VersionEdit { tone: Field::Set(Tone { wb: Field::Set(kelvin_wb(6000.0, 0.0)), ..Tone::default() }), ..VersionEdit::default() };
        let [cooler, warmer] = duel_pair(&working, DuelDim::Warmth, 0, Some(&CTX));
        assert!(wb_of(&cooler).kelvin.get().unwrap() < 6000.0);
        assert!(wb_of(&warmer).kelvin.get().unwrap() > 6000.0);
        let [c2, _] = duel_pair(&working, DuelDim::Warmth, 1, Some(&CTX));
        assert!(6000.0 - wb_of(&c2).kelvin.get().unwrap() < 6000.0 - wb_of(&cooler).kelvin.get().unwrap());
    }

    #[test]
    fn a_shift_keeps_the_rest_of_the_record() {
        let r = with_kelvin_shift(&parse_edit(Some(r#"{"engine":2,"fade":0.2,"tone":{"ev":0.5}}"#)), &CTX, -30.0);
        assert_eq!(r.engine, Field::Set(2.0));
        assert_eq!(r.fade, Field::Set(0.2));
        assert_eq!(r.tone.into_value().unwrap().ev, Field::Set(0.5));
    }

    #[test]
    fn history_names_the_light_the_tint_and_back_to_as_shot() {
        let at = |wb: serde_json::Value| parse_edit(Some(&json!({"tone": {"wb": wb}}).to_string()));
        let k = |kelvin: f64, tint: f64| serde_json::to_value(kelvin_wb(kelvin, tint)).unwrap();
        assert_eq!(describe_change(&at(json!({"temp": 0, "tint": 0})), &at(k(4800.0, 0.0)), None).label, "White balance 4800 K");
        assert_eq!(describe_change(&at(k(4800.0, 0.0)), &at(k(4800.0, 6.0)), None).label, "Tint +6");
        assert_eq!(describe_change(&at(k(4800.0, 6.0)), &at(json!({"temp": 0, "tint": 0})), None).label, "White balance as shot");
        assert_eq!(describe_change(&at(json!({"temp": 0, "tint": 0})), &at(json!({"temp": 0.3, "tint": 0})), None).label, "Temperature +0.30");
    }
}
