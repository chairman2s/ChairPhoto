//! Develop presets: one-click looks — a port of `src/modules/presets.ts`.
//!
//! A preset is a *look-only* partial edit record (tone + film-look params, never the
//! framing) applied over zero defaults, so clicking a preset replaces the look but keeps the
//! crop. Built-ins are parameter recipes; user presets are saved per catalog as one JSON
//! array under the setting [`USER_PRESETS_KEY`].
//!
//! The TS functions that did IO (`loadUserPresets`, `saveUserPresets`, `addUserPreset`,
//! `allPresets`) become pure functions over the setting's text: the caller reads and writes
//! the setting, and supplies the new preset's id (TS used `crypto.randomUUID()`).

use crate::js_compat::js_trim;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::editing::{Bw, Field, Grain, Split, Tone, VersionEdit, Wb};
use crate::js_compat;

/// The settings key the user presets live under.
pub const USER_PRESETS_KEY: &str = "basic-editor.presets";

/// A preset's group in the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresetCategory {
    Monochrome,
    Film,
    Color,
    User,
}

/// The categories in display order.
pub const PRESET_CATEGORIES: [PresetCategory; 4] =
    [PresetCategory::Monochrome, PresetCategory::Film, PresetCategory::Color, PresetCategory::User];

/// A develop preset (TS `DevelopPreset`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevelopPreset {
    /// Builtin: a stable slug ("bw-red"); user: a UUID.
    pub id: String,
    pub name: String,
    pub category: PresetCategory,
    /// Look-only fields — never crop or straighten ([`look_only`]). A [`Field`] so a stored
    /// payload this build cannot read (or a missing or `null` one) is written back as it
    /// was, as TS's `{ ...p }` kept it; [`Field::spread`] is what applying it spreads.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub edit: Field<VersionEdit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<bool>,
    /// Keys a stored preset carries that this build does not know, written back as read.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}


fn split(shadow_hue: f64, shadow_sat: f64, highlight_hue: f64, highlight_sat: f64) -> Field<Split> {
    Field::Set(Split { shadow_hue, shadow_sat, highlight_hue, highlight_sat, balance: 0.0, extra: Map::new() })
}

fn grain(amount: f64, size: f64) -> Field<Grain> {
    Field::Set(Grain { amount, size, seed: 0.0, extra: Map::new() })
}

fn wb(temp: f64) -> Field<Wb> {
    Field::Set(Wb::relative(temp, 0.0))
}

fn builtin(id: &str, name: &str, category: PresetCategory, edit: VersionEdit) -> DevelopPreset {
    DevelopPreset { id: id.into(), name: name.into(), category, edit: Field::Set(edit), builtin: Some(true), extra: Map::new() }
}

/// The built-in library, in display order. Recipes are starting points tuned by eye.
/// Tones are sparse: a recipe lists only the keys it touches.
pub fn builtin_presets() -> Vec<DevelopPreset> {
    use PresetCategory::*;
    let e = VersionEdit::default;
    let t = Tone::default;
    vec![
        // --- Monochrome styles ---
        builtin("bw-neutral", "B&W Neutral", Monochrome, VersionEdit { bw: Field::Set(Bw::mix(0.299, 0.587, 0.114)), tone: Field::Set(Tone { contrast: Field::Set(0.1), ..t() }), ..e() }),
        // Dramatic skies: reds/skin bright, blues near-black.
        builtin("bw-red", "B&W Red Filter", Monochrome, VersionEdit { bw: Field::Set(Bw::mix(0.9, 0.15, -0.05)), tone: Field::Set(Tone { contrast: Field::Set(0.2), ..t() }), ..e() }),
        builtin("bw-yellow", "B&W Yellow Filter", Monochrome, VersionEdit { bw: Field::Set(Bw::mix(0.55, 0.4, 0.05)), tone: Field::Set(Tone { contrast: Field::Set(0.12), ..t() }), ..e() }),
        // Classic for foliage and natural skin rendering.
        builtin("bw-green", "B&W Green Filter", Monochrome, VersionEdit { bw: Field::Set(Bw::mix(0.2, 0.7, 0.1)), tone: Field::Set(Tone { contrast: Field::Set(0.1), ..t() }), ..e() }),
        builtin(
            "bw-high-contrast",
            "B&W High Contrast",
            Monochrome,
            VersionEdit { bw: Field::Set(Bw::mix(0.299, 0.587, 0.114)), tone: Field::Set(Tone { contrast: Field::Set(0.45), blacks: Field::Set(-0.15), whites: Field::Set(0.15), ..t() }), ..e() },
        ),
        builtin(
            "sepia",
            "Sepia",
            Monochrome,
            VersionEdit { bw: Field::Set(Bw::mix(0.299, 0.587, 0.114)), split: split(35.0, 0.25, 45.0, 0.12), tone: Field::Set(Tone { contrast: Field::Set(0.05), ..t() }), ..e() },
        ),
        // Cool purple-blue toning in the shadows, like a selenium-toned print.
        builtin(
            "selenium",
            "Selenium",
            Monochrome,
            VersionEdit { bw: Field::Set(Bw::mix(0.299, 0.587, 0.114)), split: split(275.0, 0.15, 250.0, 0.06), tone: Field::Set(Tone { contrast: Field::Set(0.15), ..t() }), ..e() },
        ),
        // --- Film stocks ---
        // Gritty photojournalism B&W: contrasty, crushed blacks, visible grain.
        builtin(
            "tri-x",
            "Tri-X 400",
            Film,
            VersionEdit {
                bw: Field::Set(Bw::mix(0.35, 0.45, 0.2)),
                tone: Field::Set(Tone { contrast: Field::Set(0.3), blacks: Field::Set(-0.1), ..t() }),
                grain: grain(0.5, 1.2),
                ..e()
            },
        ),
        // Warm consumer negative film: golden highlights, gentle fade, light grain.
        builtin(
            "kodak-gold",
            "Kodak Gold 200",
            Film,
            VersionEdit {
                tone: Field::Set(Tone { vibrance: Field::Set(0.15), wb: wb(0.15), ..t() }),
                split: split(40.0, 0.0, 45.0, 0.08),
                fade: Field::Set(0.1),
                grain: grain(0.2, 1.0),
                ..e()
            },
        ),
        // Soft, low-contrast portrait film with warm shadows and muted saturation.
        builtin(
            "portra",
            "Portra 400",
            Film,
            VersionEdit {
                tone: Field::Set(Tone { contrast: Field::Set(-0.05), saturation: Field::Set(-0.1), shadows: Field::Set(0.1), wb: wb(0.08), ..t() }),
                split: split(20.0, 0.06, 40.0, 0.0),
                grain: grain(0.15, 1.0),
                ..e()
            },
        ),
        // Clean slide film with a slightly cool cast and blue-leaning shadows.
        builtin(
            "ektachrome",
            "Ektachrome E100",
            Film,
            VersionEdit {
                tone: Field::Set(Tone { saturation: Field::Set(0.15), contrast: Field::Set(0.15), wb: wb(-0.05), ..t() }),
                split: split(220.0, 0.05, 200.0, 0.0),
                ..e()
            },
        ),
        // Punchy, warm, deep-shadowed slide film with golden highlights.
        builtin(
            "kodachrome",
            "Kodachrome 64",
            Film,
            VersionEdit {
                tone: Field::Set(Tone { contrast: Field::Set(0.25), saturation: Field::Set(0.1), blacks: Field::Set(-0.1), wb: wb(0.05), ..t() }),
                split: split(40.0, 0.0, 50.0, 0.05),
                ..e()
            },
        ),
        // Landscape slide film: maximum colour punch.
        builtin(
            "velvia",
            "Velvia 50",
            Film,
            VersionEdit { tone: Field::Set(Tone { saturation: Field::Set(0.35), vibrance: Field::Set(0.2), contrast: Field::Set(0.2), ..t() }), ..e() },
        ),
        // --- Color looks ---
        builtin(
            "auto",
            "Auto",
            Color,
            VersionEdit { tone: Field::Set(Tone { ev: Field::Set(0.15), contrast: Field::Set(0.1), highlights: Field::Set(-0.2), shadows: Field::Set(0.15), ..t() }), ..e() },
        ),
        builtin(
            "landscape",
            "Landscape",
            Color,
            VersionEdit { tone: Field::Set(Tone { vibrance: Field::Set(0.35), contrast: Field::Set(0.1), highlights: Field::Set(-0.25), shadows: Field::Set(0.1), ..t() }), ..e() },
        ),
        builtin(
            "punch",
            "Punch",
            Color,
            VersionEdit { tone: Field::Set(Tone { contrast: Field::Set(0.3), vibrance: Field::Set(0.25), blacks: Field::Set(-0.15), ..t() }), ..e() },
        ),
        builtin(
            "faded-matte",
            "Faded Matte",
            Color,
            VersionEdit {
                tone: Field::Set(Tone { contrast: Field::Set(-0.1), saturation: Field::Set(-0.15), ..t() }),
                fade: Field::Set(0.5),
                grain: grain(0.2, 1.0),
                ..e()
            },
        ),
    ]
}

/// The user presets stored in the setting's text (`loadUserPresets`). Missing, empty,
/// invalid or non-array text gives none. Entries without a string `id` and `name` are
/// skipped; every entry is forced to category `User`, `builtin: false`. An entry's `edit`
/// is kept as a [`Field`]: a record when it is one (read like any record), otherwise —
/// missing, `null`, an array, a string — as it was, so a save writes it back unchanged.
pub fn parse_user_presets(raw: Option<&str>) -> Vec<DevelopPreset> {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else { return Vec::new() };
    let Ok(Value::Array(list)) = serde_json::from_str::<Value>(raw) else { return Vec::new() };
    list.into_iter()
        .filter_map(|p| {
            let Value::Object(mut obj) = p else { return None };
            let id = obj.get("id")?.as_str()?.to_string();
            let name = obj.get("name")?.as_str()?.to_string();
            // Field's reading never fails: a non-object payload is kept raw.
            let edit = obj.remove("edit").map_or(Field::Absent, |v| serde_json::from_value(v).unwrap_or(Field::Absent));
            for k in ["id", "name", "category", "builtin"] {
                obj.remove(k);
            }
            Some(DevelopPreset { id, name, category: PresetCategory::User, edit, builtin: Some(false), extra: obj })
        })
        .collect()
}

/// The setting's text for `list` (`saveUserPresets`: whole-array replace).
pub fn serialize_user_presets(list: &[DevelopPreset]) -> String {
    js_compat::to_json_string(&list)
}

/// A record's look, as a preset stores it: everything but the framing (crop, straighten,
/// perspective), the engine stamp, and the engine's display transform, camera match and
/// lens corrections — all of which belong to the photo, not the look. Unknown keys stay.
pub fn look_only(record: &VersionEdit) -> VersionEdit {
    VersionEdit {
        crop: Field::Absent,
        straighten: Field::Absent,
        perspective: Field::Absent,
        engine: Field::Absent,
        display: Field::Absent,
        camera_ev: Field::Absent,
        lens: Field::Absent,
        ..record.clone()
    }
}

/// The stored user presets (`stored`, the setting's text) with a new preset `id` named
/// `name` (trimmed) holding `record`'s look appended (`addUserPreset`). The caller saves
/// [`serialize_user_presets`] of the result under [`USER_PRESETS_KEY`].
pub fn add_user_preset(stored: Option<&str>, id: String, name: &str, record: &VersionEdit) -> Vec<DevelopPreset> {
    let mut list = parse_user_presets(stored);
    list.push(DevelopPreset {
        id,
        name: js_trim(name).to_string(),
        category: PresetCategory::User,
        edit: Field::Set(look_only(record)),
        builtin: None,
        extra: Map::new(),
    });
    list
}

/// The stored user presets with preset `id` renamed to `name` (trimmed; the preset
/// browser's ✎). An empty name, or an id not stored, changes nothing. The caller saves
/// [`serialize_user_presets`] of the result. Every other entry is written back as read —
/// its unknown keys and payload included; as in TS, an entry without a string `id` and
/// `name` is not one the browser can show, and is not kept.
pub fn rename_user_preset(stored: Option<&str>, id: &str, name: &str) -> Vec<DevelopPreset> {
    let name = js_trim(name);
    let mut list = parse_user_presets(stored);
    if !name.is_empty() {
        for p in list.iter_mut().filter(|p| p.id == id) {
            p.name = name.to_string();
        }
    }
    list
}

/// The stored user presets without preset `id` (the browser's ×, no confirm).
pub fn delete_user_preset(stored: Option<&str>, id: &str) -> Vec<DevelopPreset> {
    let mut list = parse_user_presets(stored);
    list.retain(|p| p.id != id);
    list
}

/// All presets in display order: the built-in groups first, then the user's.
pub fn all_presets(user: Vec<DevelopPreset>) -> Vec<DevelopPreset> {
    let mut out = builtin_presets();
    out.extend(user);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::{parse_edit, Crop, LutRef};
    use serde_json::json;

    // --- src/modules/__tests__/presetLookOnly.test.ts (1 case) ---

    #[test]
    fn look_only_keeps_tone_zones_and_looks_drops_framing_the_engine_stamp_and_its_display_transform() {
        let record = parse_edit(Some(
            &json!({
                "engine": 2,
                "display": "camera",
                "cameraEv": -1.6,
                "lens": {"builtin": true},
                "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "4:5"},
                "straighten": 1.5,
                "perspective": {"tl": [0, 0], "tr": [1, 0], "br": [1, 1], "bl": [0, 1]},
                "tone": {"ev": 0.5, "contrast": 0.2},
                "zones": [0, 0.1, 0, 0, 0, 0, 0, 0],
                "fade": 0.2,
                "lut": {"file": "portra.cube", "amount": 0.8},
            })
            .to_string(),
        ));
        let look = look_only(&record);
        assert!([look.crop.is_absent(), look.straighten.is_absent(), look.perspective.is_absent()].iter().all(|a| *a));
        assert!([look.engine.is_absent(), look.display.is_absent(), look.camera_ev.is_absent(), look.lens.is_absent()].iter().all(|a| *a));
        assert_eq!(look.tone.value().and_then(|t| t.ev.get()), Some(0.5));
        assert_eq!(look.zones, Field::Set(vec![0.0, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
        assert_eq!(look.fade, Field::Set(0.2));
        assert_eq!(look.lut, Field::Set(LutRef { file: "portra.cube".into(), amount: 0.8, extra: Map::new() }));
        assert!(record.crop.is_set(), "the input is not mutated");
    }

    // --- user-preset storage (new; the TS functions did IO and had no tests) ---

    #[test]
    fn user_presets_round_trip_through_the_setting_text() {
        let record = VersionEdit { crop: Field::Set(Crop::rect(0.0, 0.0, 0.5, 0.5)), fade: Field::Set(0.3), ..Default::default() };
        let list = add_user_preset(None, "u-1".into(), "  Mine \u{feff}", &record);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Mine");
        assert_eq!(list[0].edit.value().unwrap().crop, Field::Absent);
        let text = serialize_user_presets(&list);
        let v: Value = serde_json::from_str(&text).unwrap();
        // A new preset has no `builtin` key (TS omitted it); `edit` holds only the look.
        assert_eq!(v, json!([{"id": "u-1", "name": "Mine", "category": "User", "edit": {"fade": 0.3}}]));
        let back = parse_user_presets(Some(&text));
        assert_eq!(back[0].builtin, Some(false));
        assert_eq!(back[0].edit.value().unwrap().fade, Field::Set(0.3));
        let two = add_user_preset(Some(&text), "u-2".into(), "Two", &VersionEdit::default());
        assert_eq!(two.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["u-1", "u-2"]);
    }

    #[test]
    fn renaming_and_deleting_keep_every_other_entry_and_unknown_keys() {
        let stored = json!([
            {"id": "a", "name": "A", "category": "User", "edit": {"fade": 0.3, "future": {"k": 1}}, "note": "keep"},
            {"id": "b", "name": "B", "category": "User", "edit": "a payload this build cannot read"},
        ])
        .to_string();
        let renamed = rename_user_preset(Some(&stored), "a", "  Matte ");
        let v: Value = serde_json::from_str(&serialize_user_presets(&renamed)).unwrap();
        assert_eq!(v[0]["name"], json!("Matte"));
        assert_eq!(v[0]["note"], json!("keep"));
        assert_eq!(v[0]["edit"], json!({"fade": 0.3, "future": {"k": 1}}));
        assert_eq!(v[1]["edit"], json!("a payload this build cannot read"), "the other preset's payload as stored");
        assert_eq!(rename_user_preset(Some(&stored), "a", "   ")[0].name, "A", "an empty name changes nothing");
        let left = delete_user_preset(Some(&stored), "a");
        assert_eq!(left.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["b"]);
        let v: Value = serde_json::from_str(&serialize_user_presets(&left)).unwrap();
        assert_eq!(v[0]["edit"], json!("a payload this build cannot read"));
    }

    #[test]
    fn stored_user_presets_are_read_tolerantly() {
        assert!(parse_user_presets(None).is_empty());
        assert!(parse_user_presets(Some("")).is_empty());
        assert!(parse_user_presets(Some("{oops")).is_empty());
        assert!(parse_user_presets(Some(r#"{"id":"x"}"#)).is_empty());
        let list = parse_user_presets(Some(
            r#"[null, 3, {"id": 1, "name": "n"}, {"id": "a", "name": "A", "category": "Film", "builtin": true, "edit": {"tone": {"ev": 0.2}}, "note": "kept"}, {"id": "b", "name": "B"}]"#,
        ));
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].category, list[0].builtin), (PresetCategory::User, Some(false)));
        assert_eq!(list[0].extra.get("note"), Some(&json!("kept")));
        assert_eq!(list[1].edit, Field::Absent);
    }

    /// Codex review of c718349: loading read an unparseable `edit` as `{}`, so saving an
    /// unrelated preset persisted the loss. TS's `{ ...p, category, builtin }` keeps the
    /// payload as it was — and keeps a missing or `null` one missing or `null`.
    #[test]
    fn a_stored_preset_payload_survives_saving_another_preset() {
        let stored = r#"[{"id":"old","name":"Old","edit":[{"future":42}]}]"#;
        let list = add_user_preset(Some(stored), "new".into(), "New", &VersionEdit::default());
        let saved: Value = serde_json::from_str(&serialize_user_presets(&list)).unwrap();
        assert_eq!(saved[0]["edit"], json!([{"future": 42}]));
        for (stored, edit) in [
            (r#"[{"id":"a","name":"A","edit":null}]"#, Some(json!(null))),
            (r#"[{"id":"a","name":"A","edit":"x"}]"#, Some(json!("x"))),
            (r#"[{"id":"a","name":"A"}]"#, None),
        ] {
            let saved: Value = serde_json::from_str(&serialize_user_presets(&parse_user_presets(Some(stored)))).unwrap();
            assert_eq!(saved[0].get("edit"), edit.as_ref(), "{stored}");
        }
    }

    #[test]
    fn the_library_lists_builtins_by_category_then_the_users() {
        let all = all_presets(parse_user_presets(Some(r#"[{"id":"u","name":"U","category":"User","edit":{}}]"#)));
        assert_eq!(all.len(), 18);
        assert_eq!(all[0].id, "bw-neutral");
        assert_eq!(all.last().unwrap().id, "u");
        // Builtins never carry framing or an engine stamp.
        for p in builtin_presets() {
            let edit = p.edit.value().unwrap();
            assert_eq!(*edit, look_only(edit), "{}", p.id);
            assert_eq!(p.builtin, Some(true));
        }
    }
}
