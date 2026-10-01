//! Proof-spread and duel candidates for the Darkroom — a port of
//! `src/components/darkroom/spreads.ts` (docs/plans/darkroom). Pure record maths.
//!
//! A candidate never touches the base's framing: crop, straighten, perspective and the lens
//! corrections travel through untouched. Look cells replace the look wholesale — zones
//! included, since a fresh look deserves a flat strip — while Auto cells keep the base's
//! look and only fix its tone.

use crate::darkroom::kelvin::{with_kelvin_shift, KelvinContext, DUEL_WARMTH_MIREDS, PROOF_WARMTH_MIREDS};
use crate::editing::{Field, Tone, VersionEdit, Wb};
use crate::js_compat;
use crate::presets::{DevelopPreset, PresetCategory};
use serde_json::Map;

/// Which family a proof cell belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofGroup {
    AsShot,
    Auto,
    Film,
    Bw,
    Look,
}

/// One cell of the proof sheet.
#[derive(Clone, Debug, PartialEq)]
pub struct ProofCandidate {
    pub label: String,
    pub group: ProofGroup,
    pub record: VersionEdit,
}

/// Cells on a proof sheet.
pub const PROOF_CELLS: usize = 12;

/// The base's framing, as TS picked it: crop, perspective and lens when truthy (so a
/// `null` one is left out, a raw one kept), straighten whenever the key is there
/// (`!== undefined`, so a `null` one is copied).
fn geometry_of(base: &VersionEdit) -> VersionEdit {
    VersionEdit {
        crop: if base.crop.is_truthy() { base.crop.clone() } else { Field::Absent },
        straighten: base.straighten.clone(),
        perspective: if base.perspective.is_truthy() { base.perspective.clone() } else { Field::Absent },
        lens: if base.lens.is_truthy() { base.lens.clone() } else { Field::Absent },
        ..VersionEdit::default()
    }
}

/// Sparse-over-sparse tone merge (`b`'s keys win); absent when neither is truthy
/// (`a || b`). A raw array or string tone spreads its index keys ([`Field::spread`]).
fn merge_tone(a: &Field<Tone>, b: &Field<Tone>) -> Field<Tone> {
    if a.is_truthy() || b.is_truthy() {
        Field::Set(a.spread().merged(&b.spread()))
    } else {
        Field::Absent
    }
}

/// `t` with a relative white balance `{ temp, tint }` — the tint kept, any Kelvin mode
/// dropped (as in TS).
fn warmed(t: &Field<Tone>, temp: f64) -> Tone {
    // `t?.wb?.tint ?? 0`: a raw tint is carried as it is.
    let tint = t.value().and_then(|t| t.wb.value()).map_or(Field::Set(0.0), |w| w.tint.or_set(0.0));
    let wb = Wb { tint, ..Wb::relative(temp, 0.0) };
    Tone { wb: Field::Set(wb), ..t.spread() }
}

fn group_of(p: &DevelopPreset) -> ProofGroup {
    match p.category {
        PresetCategory::Film => ProofGroup::Film,
        PresetCategory::Monochrome => ProofGroup::Bw,
        _ => ProofGroup::Look,
    }
}

/// Round-robin across categories so the spread shows range, not one family.
fn pick_looks(presets: &[DevelopPreset], n: usize) -> Vec<&DevelopPreset> {
    use PresetCategory::*;
    let queues: Vec<Vec<&DevelopPreset>> =
        [Film, Color, Monochrome, User].iter().map(|c| presets.iter().filter(|p| p.category == *c).collect()).collect();
    let mut out = Vec::new();
    let mut round = 0;
    while out.len() < n {
        let mut added = false;
        for q in &queues {
            if let Some(p) = q.get(round) {
                if out.len() < n {
                    out.push(*p);
                    added = true;
                }
            }
        }
        if !added {
            break;
        }
        round += 1;
    }
    out
}

// ── Duels ───────────────────────────────────────────────────────────────────
// One round explores one dimension: A and B sit symmetrically around the working state,
// the step halves on every revisit (coordinate descent by eye).

/// A dimension a duel round explores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DuelDim {
    Ev,
    Warmth,
    Contrast,
    Shadows,
}

/// The duelled dimensions, in round order.
pub const DUEL_DIMS: [DuelDim; 4] = [DuelDim::Ev, DuelDim::Warmth, DuelDim::Contrast, DuelDim::Shadows];

impl DuelDim {
    /// The dimension's label (`DUEL_LABELS`).
    pub fn label(self) -> &'static str {
        match self {
            DuelDim::Ev => "Exposure",
            DuelDim::Warmth => "Warmth",
            DuelDim::Contrast => "Contrast",
            DuelDim::Shadows => "Shadows",
        }
    }

    /// First-visit step; halves per revisit.
    fn base_step(self) -> f64 {
        match self {
            DuelDim::Ev => 0.4,
            DuelDim::Warmth => 0.2,
            DuelDim::Contrast => 0.2,
            DuelDim::Shadows => 0.25,
        }
    }
}

fn clamp1(v: f64) -> f64 {
    js_compat::clamp(v, -1.0, 1.0)
}

fn with_tone(r: &VersionEdit, patch: Tone) -> VersionEdit {
    VersionEdit { tone: Field::Set(r.tone.spread().merged(&patch)), ..r.clone() }
}

/// A/B variants around `working` for `dim`; `visit` counts prior rounds on this dim.
///
/// On the RAW with an as-shot light (`kelvin` given), warmth steps stated light in mireds.
/// Otherwise warmth moves the relative `temp`, keeping `tint` and dropping any Kelvin mode.
/// A `null` `wb.temp` counts as 0, as `null - step` did in JS. Deliberate deviation: an
/// absent one counts as 0 too, where TS computed `undefined - step` = NaN and wrote `null`
/// — a record core cannot render.
pub fn duel_pair(working: &VersionEdit, dim: DuelDim, visit: i32, kelvin: Option<&KelvinContext>) -> [VersionEdit; 2] {
    let decay = 0.5f64.powi(visit.max(0));
    let step = dim.base_step() * decay;
    let t = working.tone.value_or_default();
    if let (DuelDim::Warmth, Some(ctx)) = (dim, kelvin) {
        let m = DUEL_WARMTH_MIREDS * decay;
        return [with_kelvin_shift(working, ctx, m), with_kelvin_shift(working, ctx, -m)];
    }
    let patch = |f: &dyn Fn(f64) -> Tone| [with_tone(working, f(-step)), with_tone(working, f(step))];
    match dim {
        DuelDim::Ev => {
            let ev = t.ev.num_or(0.0);
            patch(&|d| Tone { ev: Field::Set(ev + d), ..Tone::default() })
        }
        DuelDim::Warmth => {
            // `t?.wb ?? { temp: 0, tint: 0 }`; a raw (non-object) one has no members.
            let wb = match &t.wb {
                Field::Set(w) => w.clone(),
                Field::Raw(_) => Wb::default(),
                Field::Absent | Field::Null => Wb::relative(0.0, 0.0),
            };
            let temp = wb.temp.num_or(0.0);
            // `{ temp, tint: wb.tint }`: the tint as it was, `null` included.
            let relative = |temp| Wb { temp: Field::Set(temp), tint: wb.tint.clone(), mode: Field::Absent, kelvin: Field::Absent, extra: Map::new() };
            patch(&|d| Tone { wb: Field::Set(relative(clamp1(temp + d))), ..Tone::default() })
        }
        DuelDim::Contrast => {
            let c = t.contrast.num_or(0.0);
            patch(&|d| Tone { contrast: Field::Set(clamp1(c + d)), ..Tone::default() })
        }
        DuelDim::Shadows => {
            let s = t.shadows.num_or(0.0);
            patch(&|d| Tone { shadows: Field::Set(clamp1(s + d)), ..Tone::default() })
        }
    }
}

/// The spread: the current state (always first — declining is a click), three Auto cells
/// (fix, warm, cool), and looks over the preset library until the sheet is full. `auto` is
/// the `suggest_auto_tone` fragment, parsed.
///
/// The first cell reads "Current" when `base` holds anything, else "As shot". TS asked
/// `Object.keys(base).length > 0`, which also counted keys set to `undefined`; a Rust
/// record has no such keys, so this is `!base.is_empty()` — the same for any parsed record
/// (a `null` key counts, as it did in TS).
pub fn proof_spread(
    base: &VersionEdit,
    auto: &VersionEdit,
    presets: &[DevelopPreset],
    kelvin: Option<&KelvinContext>,
) -> Vec<ProofCandidate> {
    let geo = geometry_of(base);
    let auto_tone = merge_tone(&base.tone, &auto.tone);
    let auto_base = VersionEdit { tone: auto_tone.clone(), ..base.clone() };
    let cell = |label: &str, group, record| ProofCandidate { label: label.into(), group, record };
    let (warm, cool) = match kelvin {
        // On the RAW with an as-shot light, warm and cool are stated light, 30 mireds either
        // side of what the photo shows now; otherwise the relative warmth nudge.
        Some(ctx) => (
            with_kelvin_shift(&auto_base, ctx, -PROOF_WARMTH_MIREDS),
            with_kelvin_shift(&auto_base, ctx, PROOF_WARMTH_MIREDS),
        ),
        None => (
            VersionEdit { tone: Field::Set(warmed(&auto_tone, 0.35)), ..base.clone() },
            VersionEdit { tone: Field::Set(warmed(&auto_tone, -0.35)), ..base.clone() },
        ),
    };
    let mut out = vec![
        cell(if base.is_empty() { "As shot" } else { "Current" }, ProofGroup::AsShot, base.clone()),
        cell("Auto", ProofGroup::Auto, auto_base.clone()),
        cell("Auto · Warm", ProofGroup::Auto, warm),
        cell("Auto · Cool", ProofGroup::Auto, cool),
    ];
    for p in pick_looks(presets, PROOF_CELLS - out.len()) {
        // Framing + Auto's exposure + the preset's whole look; the preset's own tone keys
        // win over Auto's (a B&W recipe's contrast is part of the recipe).
        // `{ ...geo, ...p.edit, tone: … p.edit.tone }`: a raw payload spreads its index keys
        // and has no `tone`.
        let edit = p.edit.spread();
        let tone = p.edit.value().map_or(Field::Absent, |e| e.tone.clone());
        let record = VersionEdit { tone: merge_tone(&auto_tone, &tone), ..geo.overlaid(&edit) };
        out.push(cell(&p.name, group_of(p), record));
    }
    out
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/spreads.test.ts (9 cases) ---
    use super::*;
    use crate::editing::{Bw, Crop, Lens, Perspective, Split};

    fn preset(id: &str, category: PresetCategory, edit: VersionEdit) -> DevelopPreset {
        DevelopPreset { id: id.into(), name: id.into(), category, edit: Field::Set(edit), builtin: Some(true), extra: Map::new() }
    }

    fn tone(f: impl FnOnce(&mut Tone)) -> Field<Tone> {
        let mut t = Tone::default();
        f(&mut t);
        Field::Set(t)
    }

    fn many_presets() -> Vec<DevelopPreset> {
        use PresetCategory::*;
        let e = VersionEdit::default;
        vec![
            preset("portra", Film, VersionEdit { fade: Field::Set(0.1), tone: tone(|t| t.contrast = Field::Set(0.05)), ..e() }),
            preset("velvia", Film, VersionEdit { tone: tone(|t| t.saturation = Field::Set(0.4)), ..e() }),
            preset("gold", Film, e()),
            preset(
                "teal",
                Color,
                VersionEdit { split: Field::Set(Split { shadow_hue: 180.0, shadow_sat: 0.2, highlight_hue: 40.0, highlight_sat: 0.1, balance: 0.0, extra: Map::new() }), ..e() },
            ),
            preset("sunset", Color, e()),
            preset(
                "bw-red",
                Monochrome,
                VersionEdit { bw: Field::Set(Bw::mix(0.9, 0.15, -0.05)), tone: tone(|t| t.contrast = Field::Set(0.2)), ..e() },
            ),
            preset("bw-soft", Monochrome, e()),
            preset("sepia", Monochrome, e()),
            preset("mine", User, VersionEdit { vignette: Field::Set(-0.2), ..e() }),
            preset("extra1", Film, e()),
            preset("extra2", Color, e()),
            preset("extra3", Monochrome, e()),
        ]
    }

    fn auto() -> VersionEdit {
        VersionEdit { tone: tone(|t| { t.ev = Field::Set(0.5); t.contrast = Field::Set(0.1) }), ..VersionEdit::default() }
    }

    fn find<'a>(spread: &'a [ProofCandidate], label: &str) -> &'a VersionEdit {
        &spread.iter().find(|c| c.label == label).expect(label).record
    }

    fn tone_of(r: &VersionEdit) -> Tone {
        r.tone.value().cloned().unwrap_or_default()
    }

    #[test]
    fn always_deals_the_current_state_first_exactly_once() {
        let spread = proof_spread(&VersionEdit::default(), &auto(), &many_presets(), None);
        assert_eq!(spread[0].group, ProofGroup::AsShot);
        assert_eq!(spread[0].label, "As shot");
        assert_eq!(spread.iter().filter(|c| c.group == ProofGroup::AsShot).count(), 1);
        let edited = proof_spread(&VersionEdit { tone: tone(|t| t.ev = Field::Set(1.0)), ..VersionEdit::default() }, &auto(), &many_presets(), None);
        assert_eq!(edited[0].label, "Current");
    }

    #[test]
    fn caps_the_sheet_at_proof_cells() {
        assert_eq!(proof_spread(&VersionEdit::default(), &auto(), &many_presets(), None).len(), PROOF_CELLS);
    }

    #[test]
    fn copies_the_bases_framing_into_every_candidate_untouched() {
        let base = VersionEdit {
            crop: Field::Set(Crop { aspect: Field::Set("1:1".into()), ..Crop::rect(0.1, 0.2, 0.5, 0.5) }),
            straighten: Field::Set(1.5),
            perspective: Field::Set(Perspective { tl: [0.0, 0.0], tr: [1.0, 0.0], br: [1.0, 1.0], bl: [0.0, 1.0], aspect: Field::Absent, extra: Map::new() }),
            lens: Field::Set(Lens { builtin: true, extra: Map::new() }),
            ..VersionEdit::default()
        };
        for c in proof_spread(&base, &auto(), &many_presets(), None) {
            assert_eq!(c.record.crop, base.crop);
            assert_eq!(c.record.straighten, Field::Set(1.5));
            assert_eq!(c.record.perspective, base.perspective);
            assert_eq!(c.record.lens, Field::Set(Lens { builtin: true, extra: Map::new() }), "adopting a proof keeps the lens correction");
        }
    }

    #[test]
    fn keeps_the_base_look_and_zones_on_auto_cells_drops_them_on_look_cells() {
        let base = VersionEdit { zones: Field::Set(vec![0.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), fade: Field::Set(0.3), ..VersionEdit::default() };
        let spread = proof_spread(&base, &auto(), &many_presets(), None);
        let a = find(&spread, "Auto");
        assert_eq!(a.zones, base.zones);
        assert_eq!(a.fade, Field::Set(0.3));
        let look = find(&spread, "portra");
        assert_eq!(look.zones, Field::Absent);
        assert_eq!(look.fade, Field::Set(0.1)); // the preset's, not the base's
    }

    #[test]
    fn applies_the_auto_fragment_with_a_presets_own_tone_keys_winning() {
        let spread = proof_spread(&VersionEdit::default(), &auto(), &many_presets(), None);
        assert_eq!(tone_of(find(&spread, "Auto")).ev, Field::Set(0.5));
        let bw_red = tone_of(find(&spread, "bw-red"));
        assert_eq!(bw_red.ev, Field::Set(0.5)); // auto's exposure carries in
        assert_eq!(bw_red.contrast, Field::Set(0.2)); // the recipe's contrast wins
    }

    #[test]
    fn warm_and_cool_cells_differ_only_in_white_balance() {
        let spread = proof_spread(&VersionEdit::default(), &auto(), &many_presets(), None);
        let warm = tone_of(find(&spread, "Auto · Warm"));
        let cool = tone_of(find(&spread, "Auto · Cool"));
        assert!((warm.wb.value().unwrap().temp.get().unwrap() - 0.35).abs() < 0.005);
        assert!((cool.wb.value().unwrap().temp.get().unwrap() + 0.35).abs() < 0.005);
        assert_eq!(warm.ev, cool.ev);
    }

    #[test]
    fn duel_pair_differs_only_in_its_dimension_symmetrically() {
        let working = VersionEdit {
            crop: Field::Set(Crop::rect(0.0, 0.0, 1.0, 1.0)),
            zones: Field::Set(vec![0.0, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            tone: tone(|t| {
                t.ev = Field::Set(0.5);
                t.contrast = Field::Set(0.1);
                t.wb = Field::Set(Wb::relative(0.1, -0.05));
            }),
            ..VersionEdit::default()
        };
        for dim in DUEL_DIMS {
            let [a, b] = duel_pair(&working, dim, 0, None);
            // Framing, zones, and look travel through untouched on both sides.
            assert_eq!(a.crop, working.crop);
            assert_eq!(b.zones, working.zones);
            // Exactly the duelled field differs between A and B.
            let (ta, tb) = (tone_of(&a), tone_of(&b));
            let mut diff = Vec::new();
            if ta.ev != tb.ev {
                diff.push(DuelDim::Ev);
            }
            if ta.contrast != tb.contrast {
                diff.push(DuelDim::Contrast);
            }
            if ta.shadows != tb.shadows {
                diff.push(DuelDim::Shadows);
            }
            let temp_differs = ta.wb.value().and_then(|w| w.temp.get()) != tb.wb.value().and_then(|w| w.temp.get());
            if dim == DuelDim::Warmth {
                assert!(diff.is_empty());
                assert!(temp_differs);
                assert_eq!(ta.wb.value().unwrap().tint, Field::Set(-0.05)); // tint rides along unchanged
            } else {
                assert_eq!(diff, [dim]);
                assert!(!temp_differs);
            }
        }
        // Symmetry around the working value.
        let [a, b] = duel_pair(&working, DuelDim::Ev, 0, None);
        assert!(((tone_of(&a).ev.get().unwrap() + tone_of(&b).ev.get().unwrap()) / 2.0 - 0.5).abs() < 0.005);
    }

    #[test]
    fn duel_pairs_step_decays_with_each_revisit() {
        let spread_at = |visit| {
            let [a, b] = duel_pair(&VersionEdit::default(), DuelDim::Ev, visit, None);
            (tone_of(&b).ev.get().unwrap() - tone_of(&a).ev.get().unwrap()).abs()
        };
        assert!((spread_at(1) - spread_at(0) / 2.0).abs() < 0.005);
        assert!((spread_at(3) - spread_at(0) / 8.0).abs() < 0.005);
    }

    /// Explicit `null` (review finding, #103): a record holding only a null key is
    /// "Current", as `Object.keys` saw it; a null crop is falsy and not copied as framing,
    /// while a null straighten passes `!== undefined` and is; history reads null as 0.
    #[test]
    fn explicit_null_keys_follow_the_ts_reads() {
        let base = crate::editing::parse_edit(Some(r#"{"bw":null,"crop":null,"straighten":null}"#));
        let spread = proof_spread(&base, &auto(), &many_presets(), None);
        assert_eq!(spread[0].label, "Current");
        assert_eq!(spread[0].record.bw, Field::Null);
        let look = find(&spread, "portra");
        assert_eq!((look.crop.clone(), look.straighten.clone(), look.bw.clone()), (Field::Absent, Field::Null, Field::Absent));
        let change = crate::darkroom::history::describe_change(&base, &VersionEdit::default(), None);
        assert_eq!(change.key, "none");
    }

    /// Codex review of c718349: a raw array or string tone was replaced by an empty tone in
    /// a tone adjustment. TS spreads it (`{ ...(r.tone ?? {}), ...patch }`), copying index
    /// keys; node: duel → {"tone":{"0":{"future":42},"ev":-0.4}}.
    #[test]
    fn a_raw_array_or_string_tone_spreads_its_index_keys() {
        use serde_json::{json, Value};
        let working = crate::editing::parse_edit(Some(r#"{"tone":[{"future":42}]}"#));
        let [a, b] = duel_pair(&working, DuelDim::Ev, 0, None);
        let json = |r: &VersionEdit| serde_json::from_str::<Value>(&r.to_json()).unwrap();
        assert_eq!(json(&a), json!({"tone": {"0": {"future": 42}, "ev": -0.4}}));
        assert_eq!(json(&b), json!({"tone": {"0": {"future": 42}, "ev": 0.4}}));
        // A string spreads its characters; Auto merges over it the same way.
        let spread = proof_spread(&crate::editing::parse_edit(Some(r#"{"tone":"ab"}"#)), &auto(), &[], None);
        assert_eq!(json(&spread[1].record)["tone"], json!({"0": "a", "1": "b", "ev": 0.5, "contrast": 0.1}));
    }

    #[test]
    fn spreads_looks_across_categories_round_robin() {
        let spread = proof_spread(&VersionEdit::default(), &auto(), &many_presets(), None);
        let looks: Vec<&str> = spread[4..].iter().map(|c| c.label.as_str()).collect();
        // First round: one per category in order Film, Color, Monochrome, User.
        assert_eq!(&looks[..4], ["portra", "teal", "bw-red", "mine"]);
    }
}
