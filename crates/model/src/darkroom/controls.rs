//! The edit controls' record maths — the state transitions of `ToneRail` and `EffectsRail`
//! in `src/components/EditControls.tsx` and of the bridge in `DarkroomView.tsx`, as pure
//! functions over [`VersionEdit`].
//!
//! The rails speak a decomposed dialect: a full [`Tone`] (every slider present, white
//! balance whole) and a [`Look`] with fade and vignette defaulted. [`tone_of`] and
//! [`look_of`] derive it from the working record ("derived values go down"), and every
//! control returns the next record ("updates merge back"), exactly as the React closures
//! did — so a tone slider writes the whole bridged tone back (`onTone({ ...tone, [key]: v })`),
//! and a look control rewrites all six look fields through `lookFields`. Unknown keys ride
//! along in each struct's `extra` (see [`crate::editing`]).

use serde_json::Map;

use crate::darkroom::kelvin::{kelvin_wb, slider_to_kelvin, wb_shown, KelvinContext, WbShown};
use crate::editing::{Bw, Field, Grain, Look, LutRef, Split, Tone, VersionEdit, Wb};

/// A tone or colour slider's key in the record's `tone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToneKey {
    Ev,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    Vibrance,
    Saturation,
}

impl ToneKey {
    pub fn field(self, t: &Tone) -> &Field<f64> {
        match self {
            ToneKey::Ev => &t.ev,
            ToneKey::Contrast => &t.contrast,
            ToneKey::Highlights => &t.highlights,
            ToneKey::Shadows => &t.shadows,
            ToneKey::Whites => &t.whites,
            ToneKey::Blacks => &t.blacks,
            ToneKey::Vibrance => &t.vibrance,
            ToneKey::Saturation => &t.saturation,
        }
    }

    fn field_mut(self, t: &mut Tone) -> &mut Field<f64> {
        match self {
            ToneKey::Ev => &mut t.ev,
            ToneKey::Contrast => &mut t.contrast,
            ToneKey::Highlights => &mut t.highlights,
            ToneKey::Shadows => &mut t.shadows,
            ToneKey::Whites => &mut t.whites,
            ToneKey::Blacks => &mut t.blacks,
            ToneKey::Vibrance => &mut t.vibrance,
            ToneKey::Saturation => &mut t.saturation,
        }
    }

    /// The record key (`"ev"` …), for element ids.
    pub fn key(self) -> &'static str {
        match self {
            ToneKey::Ev => "ev",
            ToneKey::Contrast => "contrast",
            ToneKey::Highlights => "highlights",
            ToneKey::Shadows => "shadows",
            ToneKey::Whites => "whites",
            ToneKey::Blacks => "blacks",
            ToneKey::Vibrance => "vibrance",
            ToneKey::Saturation => "saturation",
        }
    }
}

/// A slider row: what it moves, its label and range. Every tone/colour slider steps 0.05.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderDef<K> {
    pub key: K,
    pub label: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

const fn tone_slider(key: ToneKey, label: &'static str, min: f64, max: f64) -> SliderDef<ToneKey> {
    SliderDef { key, label, min, max, step: 0.05 }
}

/// `TONE_SLIDERS`: the Tone group.
pub const TONE_SLIDERS: [SliderDef<ToneKey>; 6] = [
    tone_slider(ToneKey::Ev, "Exposure", -3.0, 3.0),
    tone_slider(ToneKey::Contrast, "Contrast", -1.0, 1.0),
    tone_slider(ToneKey::Highlights, "Highlights", -1.0, 1.0),
    tone_slider(ToneKey::Shadows, "Shadows", -1.0, 1.0),
    tone_slider(ToneKey::Whites, "Whites", -1.0, 1.0),
    tone_slider(ToneKey::Blacks, "Blacks", -1.0, 1.0),
];

/// `COLOR_SLIDERS`: the Color group.
pub const COLOR_SLIDERS: [SliderDef<ToneKey>; 2] =
    [tone_slider(ToneKey::Vibrance, "Vibrance", -1.0, 1.0), tone_slider(ToneKey::Saturation, "Saturation", -1.0, 1.0)];

/// The relative white balance sliders' range and step (−1..1, 0.05).
pub const WB_RELATIVE: (f64, f64, f64) = (-1.0, 1.0, 0.05);

/// An Effects slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKey {
    Fade,
    Vignette,
    Grain,
    GrainSize,
}

impl EffectKey {
    pub fn key(self) -> &'static str {
        match self {
            EffectKey::Fade => "fade",
            EffectKey::Vignette => "vignette",
            EffectKey::Grain => "grain",
            EffectKey::GrainSize => "grain-size",
        }
    }

    /// What a double-click resets it to (grain size 1, the rest 0).
    pub fn reset_value(self) -> f64 {
        if self == EffectKey::GrainSize {
            1.0
        } else {
            0.0
        }
    }
}

/// The Effects sliders, in rail order.
pub const EFFECT_SLIDERS: [SliderDef<EffectKey>; 4] = [
    SliderDef { key: EffectKey::Fade, label: "Fade", min: 0.0, max: 1.0, step: 0.05 },
    SliderDef { key: EffectKey::Vignette, label: "Vignette", min: -1.0, max: 1.0, step: 0.05 },
    SliderDef { key: EffectKey::Grain, label: "Grain", min: 0.0, max: 1.0, step: 0.05 },
    SliderDef { key: EffectKey::GrainSize, label: "Grain size", min: 0.5, max: 3.0, step: 0.05 },
];

/// A split-toning slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitKey {
    ShadowHue,
    ShadowSat,
    HighlightHue,
    HighlightSat,
    Balance,
}

impl SplitKey {
    pub fn key(self) -> &'static str {
        match self {
            SplitKey::ShadowHue => "shadow_hue",
            SplitKey::ShadowSat => "shadow_sat",
            SplitKey::HighlightHue => "highlight_hue",
            SplitKey::HighlightSat => "highlight_sat",
            SplitKey::Balance => "balance",
        }
    }

    /// A hue slider (0–360°): shown in degrees, and a double-click leaves it alone.
    pub fn is_hue(self) -> bool {
        matches!(self, SplitKey::ShadowHue | SplitKey::HighlightHue)
    }

    fn get(self, s: &Split) -> f64 {
        match self {
            SplitKey::ShadowHue => s.shadow_hue,
            SplitKey::ShadowSat => s.shadow_sat,
            SplitKey::HighlightHue => s.highlight_hue,
            SplitKey::HighlightSat => s.highlight_sat,
            SplitKey::Balance => s.balance,
        }
    }

    fn set(self, s: &mut Split, v: f64) {
        match self {
            SplitKey::ShadowHue => s.shadow_hue = v,
            SplitKey::ShadowSat => s.shadow_sat = v,
            SplitKey::HighlightHue => s.highlight_hue = v,
            SplitKey::HighlightSat => s.highlight_sat = v,
            SplitKey::Balance => s.balance = v,
        }
    }

    /// The value shown when the record has no split toning (hues 35° / 45°, the rest 0).
    pub fn default_value(self) -> f64 {
        match self {
            SplitKey::ShadowHue => 35.0,
            SplitKey::HighlightHue => 45.0,
            _ => 0.0,
        }
    }
}

/// The split-toning sliders, in rail order.
pub const SPLIT_SLIDERS: [SliderDef<SplitKey>; 5] = [
    SliderDef { key: SplitKey::ShadowHue, label: "Shadow hue", min: 0.0, max: 360.0, step: 5.0 },
    SliderDef { key: SplitKey::ShadowSat, label: "Shadow sat", min: 0.0, max: 1.0, step: 0.02 },
    SliderDef { key: SplitKey::HighlightHue, label: "Highlight hue", min: 0.0, max: 360.0, step: 5.0 },
    SliderDef { key: SplitKey::HighlightSat, label: "Highlight sat", min: 0.0, max: 1.0, step: 0.02 },
    SliderDef { key: SplitKey::Balance, label: "Balance", min: -1.0, max: 1.0, step: 0.05 },
];

// ── The bridge ───────────────────────────────────────────────────────────────

/// `{ ...a, ...b }` over a white balance: `b`'s present keys win.
fn wb_overlaid(a: &Wb, b: &Wb) -> Wb {
    let mut extra = a.extra.clone();
    extra.extend(b.extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    Wb {
        temp: a.temp.overlaid(&b.temp),
        tint: a.tint.overlaid(&b.tint),
        mode: a.mode.overlaid(&b.mode),
        kelvin: a.kelvin.overlaid(&b.kelvin),
        extra,
    }
}

/// The rail's tone: `{ ...ZERO_TONE, ...working.tone, wb: { ...ZERO_TONE.wb, ...working.tone?.wb } }`.
pub fn tone_of(working: &VersionEdit) -> Tone {
    let mut tone = Tone::zero().merged(&working.tone.spread());
    // `working.tone?.wb`: a raw (non-object) tone has no members.
    let wb = working.tone.value().map(|t| t.wb.spread()).unwrap_or_default();
    tone.wb = Field::Set(wb_overlaid(&Wb::relative(0.0, 0.0), &wb));
    tone
}

/// The rail's white balance (always present in [`tone_of`]).
pub fn wb_of(tone: &Tone) -> Wb {
    tone.wb.value().cloned().unwrap_or_else(|| Wb::relative(0.0, 0.0))
}

/// `onTone(t)`: the record with its tone replaced.
pub fn with_tone(working: &VersionEdit, tone: Tone) -> VersionEdit {
    VersionEdit { tone: Field::Set(tone), ..working.clone() }
}

/// A slider's value as shown (`tone[key].toFixed(2)`, absent as 0, raw coerced).
pub fn tone_value(tone: &Tone, key: ToneKey) -> f64 {
    key.field(tone).num_or(0.0)
}

/// A tone or colour slider moved (or was double-clicked back to 0).
pub fn set_tone_key(working: &VersionEdit, key: ToneKey, v: f64) -> VersionEdit {
    let mut tone = tone_of(working);
    *key.field_mut(&mut tone) = Field::Set(v);
    with_tone(working, tone)
}

/// A relative white-balance slider (`temp` or `tint`) moved. Keeps an explicit `"relative"`
/// mode, so the rail does not flip back to Kelvin when the slider returns to zero; drops a
/// Kelvin light (`{ temp, tint, ...mode?, [key]: v }`).
pub fn set_wb_relative(working: &VersionEdit, tint: bool, v: f64) -> VersionEdit {
    let tone = tone_of(working);
    let wb = wb_of(&tone);
    let next = Wb {
        temp: if tint { wb.temp.clone() } else { Field::Set(v) },
        tint: if tint { Field::Set(v) } else { wb.tint.clone() },
        mode: if wb.mode.as_deref() == Some("relative") { Field::Set("relative".into()) } else { Field::Absent },
        kelvin: Field::Absent,
        extra: Map::new(),
    };
    with_tone(working, Tone { wb: Field::Set(next), ..tone })
}

/// Back to as-shot (double-click on a Kelvin slider): a blank white balance renders exactly
/// as the camera's light.
pub fn wb_as_shot(working: &VersionEdit) -> VersionEdit {
    let tone = tone_of(working);
    with_tone(working, Tone { wb: Field::Set(Wb::relative(0.0, 0.0)), ..tone })
}

/// What the white-balance group shows for this record and context.
pub fn wb_shown_for(working: &VersionEdit, kelvin: Option<&KelvinContext>) -> WbShown {
    let tone = tone_of(working);
    wb_shown(tone.wb.value(), kelvin)
}

/// The K/± button: Kelvin → relative (`{temp: 0, tint: 0, mode: "relative"}`), relative →
/// the as-shot light in Kelvin. Without a Kelvin context there is no button: unchanged.
pub fn toggle_wb_mode(working: &VersionEdit, kelvin: Option<&KelvinContext>) -> VersionEdit {
    let Some(ctx) = kelvin else { return working.clone() };
    let tone = tone_of(working);
    let wb = match wb_shown(tone.wb.value(), Some(ctx)) {
        WbShown::Kelvin { .. } => Wb { mode: Field::Set("relative".into()), ..Wb::relative(0.0, 0.0) },
        WbShown::Relative => kelvin_wb(ctx.as_shot.kelvin, ctx.as_shot.tint),
    };
    with_tone(working, Tone { wb: Field::Set(wb), ..tone })
}

/// The Kelvin temperature slider moved to `pos` (0..`SLIDER_STEPS`); the tint shown stays.
pub fn set_kelvin_slider(working: &VersionEdit, kelvin: Option<&KelvinContext>, pos: f64) -> VersionEdit {
    let tint = match wb_shown_for(working, kelvin) {
        WbShown::Kelvin { tint, .. } => tint,
        WbShown::Relative => 0.0,
    };
    let tone = tone_of(working);
    with_tone(working, Tone { wb: Field::Set(kelvin_wb(slider_to_kelvin(pos), tint)), ..tone })
}

/// The Kelvin tint slider moved; the light shown stays.
pub fn set_kelvin_tint(working: &VersionEdit, kelvin: Option<&KelvinContext>, tint: f64) -> VersionEdit {
    let k = match wb_shown_for(working, kelvin) {
        WbShown::Kelvin { kelvin, .. } => kelvin,
        WbShown::Relative => return working.clone(),
    };
    let tone = tone_of(working);
    with_tone(working, Tone { wb: Field::Set(kelvin_wb(k, tint)), ..tone })
}

/// The rail's look: `{ ...ZERO_LOOK, bw, split, grain, fade: fade ?? 0, vignette: vignette ?? 0, lut }`.
pub fn look_of(working: &VersionEdit) -> Look {
    Look {
        bw: working.bw.clone(),
        split: working.split.clone(),
        grain: working.grain.clone(),
        fade: working.fade.or_set(0.0),
        vignette: working.vignette.or_set(0.0),
        lut: working.lut.clone(),
    }
}

/// `onLook(l)`: every look field rewritten through `lookFields` (defaults dropped).
pub fn with_look(working: &VersionEdit, look: &Look) -> VersionEdit {
    working.with_look(look)
}

/// The grain amount and size shown (`look.grain?.amount ?? 0`, `look.grain?.size ?? 1`).
pub fn grain_of(look: &Look) -> (f64, f64) {
    look.grain.value().map_or((0.0, 1.0), |g| (g.amount, g.size))
}

/// An Effects slider's value as shown.
pub fn effect_value(look: &Look, key: EffectKey) -> f64 {
    let (amount, size) = grain_of(look);
    match key {
        EffectKey::Fade => look.fade.num_or(0.0),
        EffectKey::Vignette => look.vignette.num_or(0.0),
        EffectKey::Grain => amount,
        EffectKey::GrainSize => size,
    }
}

/// An Effects slider moved (or was double-clicked back to [`EffectKey::reset_value`]).
/// Grain is kept only with a positive amount, with seed 0.
pub fn set_effect(working: &VersionEdit, key: EffectKey, v: f64) -> VersionEdit {
    let mut look = look_of(working);
    let (amount, size) = grain_of(&look);
    let grain = |amount: f64, size: f64| {
        if amount > 0.0 {
            Field::Set(Grain { amount, size, seed: 0.0, extra: Map::new() })
        } else {
            Field::Absent
        }
    };
    match key {
        EffectKey::Fade => look.fade = Field::Set(v),
        EffectKey::Vignette => look.vignette = Field::Set(v),
        EffectKey::Grain => look.grain = grain(v, size),
        EffectKey::GrainSize => look.grain = grain(amount, v),
    }
    with_look(working, &look)
}

/// The Color chip (`None`) or a B&W filter chip.
pub fn set_bw(working: &VersionEdit, bw: Option<Bw>) -> VersionEdit {
    let mut look = look_of(working);
    look.bw = bw.into();
    with_look(working, &look)
}

/// Whether a B&W filter chip is the current mix (each weight within 0.01).
pub fn bw_filter_active(look: &Look, f: &Bw) -> bool {
    look.bw.value().is_some_and(|b| (b.r - f.r).abs() < 0.01 && (b.g - f.g).abs() < 0.01 && (b.b - f.b).abs() < 0.01)
}

/// Whether the Color chip is on (`!look.bw`: absent or `null`).
pub fn is_colour(look: &Look) -> bool {
    !look.bw.is_truthy()
}

/// A split-toning slider's value as shown (`look.split?.[key] ?? default`).
pub fn split_value(look: &Look, key: SplitKey) -> f64 {
    look.split.value().map_or(key.default_value(), |s| key.get(s))
}

/// A split-toning slider moved: the split starts from the defaults (hues 35°/45°) and the
/// record's own split, then takes the key. (A split whose JSON lacked a key reads it as
/// core's default 0, not the TS spread's 35/45: the record type cannot tell them apart.
/// Records the app writes always carry all five keys.)
pub fn set_split(working: &VersionEdit, key: SplitKey, v: f64) -> VersionEdit {
    let mut look = look_of(working);
    let mut split = look.split.value().cloned().unwrap_or_else(|| Split {
        shadow_hue: 35.0,
        shadow_sat: 0.0,
        highlight_hue: 45.0,
        highlight_sat: 0.0,
        balance: 0.0,
        extra: Map::new(),
    });
    key.set(&mut split, v);
    look.split = Field::Set(split);
    with_look(working, &look)
}

/// The LUT selector: a file (amount 1) or None.
pub fn set_lut(working: &VersionEdit, file: Option<&str>) -> VersionEdit {
    let mut look = look_of(working);
    look.lut = match file {
        Some(f) if !f.is_empty() => Field::Set(LutRef { file: f.to_string(), amount: 1.0, extra: Map::new() }),
        _ => Field::Absent,
    };
    with_look(working, &look)
}

/// The LUT amount slider (`{ ...look.lut, amount }`); nothing without a LUT.
pub fn set_lut_amount(working: &VersionEdit, amount: f64) -> VersionEdit {
    let mut look = look_of(working);
    let Some(lut) = look.lut.value().cloned() else { return working.clone() };
    look.lut = Field::Set(LutRef { amount, ..lut });
    with_look(working, &look)
}

/// The LUT selector's options: `(file, label)` for each file in the folder (`.cube`
/// dropped from the label), then the record's own LUT marked "(missing)" when the folder
/// does not have it.
pub fn lut_options(luts: &[String], look: &Look) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = luts.iter().map(|f| (f.clone(), strip_cube(f))).collect();
    if let Some(lut) = look.lut.value() {
        if !luts.contains(&lut.file) {
            out.push((lut.file.clone(), format!("{} (missing)", lut.file)));
        }
    }
    out
}

/// `f.replace(/\.cube$/i, "")`.
fn strip_cube(f: &str) -> String {
    let n = f.len();
    if n >= 5 && f.is_char_boundary(n - 5) && f[n - 5..].eq_ignore_ascii_case(".cube") {
        f[..n - 5].to_string()
    } else {
        f.to_string()
    }
}

/// The tone strip changed: the record's `zones`.
pub fn set_zones(working: &VersionEdit, zones: Vec<f64>) -> VersionEdit {
    VersionEdit { zones: Field::Set(zones), ..working.clone() }
}

/// The record's zones, when they are a list of numbers.
pub fn zones_of(working: &VersionEdit) -> Option<&[f64]> {
    working.zones.value().map(Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::darkroom::kelvin::{AsShotWb, WbPrefer, KELVIN_TINT_RANGE, SLIDER_STEPS};
    use crate::editing::{bw_filters, parse_edit};
    use serde_json::{json, Value};

    fn rec(v: Value) -> VersionEdit {
        parse_edit(Some(&v.to_string()))
    }

    fn js(e: &VersionEdit) -> Value {
        serde_json::from_str(&e.to_json()).unwrap()
    }

    const CTX: KelvinContext = KelvinContext { as_shot: AsShotWb { kelvin: 5300.0, tint: 2.0 }, prefer: WbPrefer::Kelvin };

    #[test]
    fn a_tone_slider_writes_the_whole_bridged_tone_and_keeps_the_rest() {
        let w = rec(json!({"tone": {"ev": 0.5, "x": 1}, "crop": {"x": 0.1, "y": 0, "w": 0.8, "h": 1}, "future": true}));
        let next = set_tone_key(&w, ToneKey::Contrast, 0.25);
        assert_eq!(
            js(&next)["tone"],
            json!({"ev": 0.5, "contrast": 0.25, "highlights": 0, "shadows": 0, "whites": 0, "blacks": 0,
                   "vibrance": 0, "saturation": 0, "wb": {"temp": 0, "tint": 0}, "x": 1})
        );
        assert_eq!(js(&next)["crop"], js(&w)["crop"]);
        assert_eq!(js(&next)["future"], json!(true), "an unknown key survives a slider move");
        assert_eq!(tone_value(&tone_of(&next), ToneKey::Contrast), 0.25);
        // Double-click: back to 0.
        assert_eq!(tone_value(&tone_of(&set_tone_key(&next, ToneKey::Ev, 0.0)), ToneKey::Ev), 0.0);
    }

    #[test]
    fn relative_white_balance_keeps_an_explicit_relative_mode_and_drops_kelvin() {
        let w = rec(json!({"tone": {"wb": {"temp": 0.2, "tint": 0.1, "mode": "relative"}}}));
        assert_eq!(js(&set_wb_relative(&w, false, 0.0))["tone"]["wb"], json!({"temp": 0, "tint": 0.1, "mode": "relative"}));
        let k = rec(json!({"tone": {"wb": {"temp": 0, "tint": 3, "mode": "kelvin", "kelvin": 4000}}}));
        assert_eq!(js(&set_wb_relative(&k, true, 0.5))["tone"]["wb"], json!({"temp": 0, "tint": 0.5}));
    }

    #[test]
    fn kelvin_controls_state_the_light_and_toggle_the_mode() {
        let w = VersionEdit::default();
        // A fresh RAW edit with a Kelvin preference shows the as-shot light.
        assert_eq!(wb_shown_for(&w, Some(&CTX)), WbShown::Kelvin { kelvin: 5300.0, tint: 2.0 });
        let moved = set_kelvin_slider(&w, Some(&CTX), SLIDER_STEPS);
        assert_eq!(js(&moved)["tone"]["wb"], json!({"temp": 0, "tint": 2, "mode": "kelvin", "kelvin": 12000}));
        let tinted = set_kelvin_tint(&moved, Some(&CTX), -KELVIN_TINT_RANGE);
        assert_eq!(js(&tinted)["tone"]["wb"]["tint"], json!(-50));
        assert_eq!(js(&tinted)["tone"]["wb"]["kelvin"], json!(12000));
        // K → ±, ± → the as-shot light.
        let rel = toggle_wb_mode(&tinted, Some(&CTX));
        assert_eq!(js(&rel)["tone"]["wb"], json!({"temp": 0, "tint": 0, "mode": "relative"}));
        assert_eq!(wb_shown_for(&rel, Some(&CTX)), WbShown::Relative);
        let back = toggle_wb_mode(&rel, Some(&CTX));
        assert_eq!(js(&back)["tone"]["wb"], json!({"temp": 0, "tint": 2, "mode": "kelvin", "kelvin": 5300}));
        // Double-click: as shot.
        assert_eq!(js(&wb_as_shot(&back))["tone"]["wb"], json!({"temp": 0, "tint": 0}));
        // No context: no button, no change.
        assert_eq!(toggle_wb_mode(&back, None), back);
    }

    #[test]
    fn effects_rewrite_the_look_and_drop_defaults() {
        let w = rec(json!({"tone": {"ev": 1}, "fade": 0.2, "lut": {"file": "a.cube", "amount": 0.5}}));
        let v = set_effect(&w, EffectKey::Vignette, -0.3);
        assert_eq!(js(&v), json!({"tone": {"ev": 1}, "fade": 0.2, "vignette": -0.3, "lut": {"file": "a.cube", "amount": 0.5}}));
        let faded_out = set_effect(&v, EffectKey::Fade, 0.0);
        assert!(js(&faded_out).get("fade").is_none(), "a zero fade is dropped");
        let grain = set_effect(&w, EffectKey::Grain, 0.4);
        assert_eq!(js(&grain)["grain"], json!({"amount": 0.4, "size": 1, "seed": 0}));
        let sized = set_effect(&grain, EffectKey::GrainSize, 2.0);
        assert_eq!(js(&sized)["grain"], json!({"amount": 0.4, "size": 2, "seed": 0}));
        assert!(js(&set_effect(&sized, EffectKey::Grain, 0.0)).get("grain").is_none(), "no grain at amount 0");
        assert!(js(&set_effect(&w, EffectKey::GrainSize, 2.0)).get("grain").is_none(), "size alone adds no grain");
        assert_eq!(effect_value(&look_of(&sized), EffectKey::GrainSize), 2.0);
        assert_eq!(EffectKey::GrainSize.reset_value(), 1.0);
    }

    #[test]
    fn bw_chips_split_toning_and_luts() {
        let w = VersionEdit::default();
        let red = &bw_filters()[1];
        let bw = set_bw(&w, Some(red.bw.clone()));
        assert!(bw_filter_active(&look_of(&bw), &red.bw));
        assert!(!bw_filter_active(&look_of(&bw), &bw_filters()[0].bw));
        assert!(!is_colour(&look_of(&bw)));
        assert!(is_colour(&look_of(&set_bw(&bw, None))));
        assert!(js(&set_bw(&bw, None)).get("bw").is_none());

        assert_eq!(split_value(&look_of(&w), SplitKey::ShadowHue), 35.0);
        let split = set_split(&w, SplitKey::ShadowSat, 0.3);
        assert_eq!(
            js(&split)["split"],
            json!({"shadow_hue": 35, "shadow_sat": 0.3, "highlight_hue": 45, "highlight_sat": 0, "balance": 0})
        );

        let lut = set_lut(&w, Some("Kodak.CUBE"));
        assert_eq!(js(&lut)["lut"], json!({"file": "Kodak.CUBE", "amount": 1}));
        assert_eq!(js(&set_lut_amount(&lut, 0.4))["lut"]["amount"], json!(0.4));
        assert!(js(&set_lut(&lut, None)).get("lut").is_none());
        assert_eq!(set_lut_amount(&w, 0.4), w, "no LUT, no amount");
        let luts = vec!["a.cube".to_string(), "b.CUBE".into()];
        assert_eq!(lut_options(&luts, &look_of(&lut)), vec![
            ("a.cube".into(), "a".into()),
            ("b.CUBE".into(), "b".into()),
            ("Kodak.CUBE".into(), "Kodak.CUBE (missing)".into()),
        ]);
    }

    #[test]
    fn zones_go_into_the_record() {
        let w = rec(json!({"zones": [0, 0, 0, 0, 0, 0, 0, 0]}));
        let z = set_zones(&w, crate::darkroom::tone_strip::apply_zone_drag(zones_of(&w), 2, 0.5));
        assert_eq!(js(&z)["zones"], json!([0, 0, 0.5, 0, 0, 0, 0, 0]));
    }
}
