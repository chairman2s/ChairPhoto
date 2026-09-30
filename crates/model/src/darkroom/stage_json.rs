//! The record the Darkroom stage renders — a port of `src/components/darkroom/stageJson.ts`.

use crate::editing::{Field, VersionEdit};

/// The stage shows the working record WITHOUT its crop — the crop is an interactive
/// overlay — and un-warped while the perspective handles are up, because the handles aim
/// at the original's corners. Masses and the loupe print use the full record.
///
/// Returns the `edit_json` text for the render request ([`VersionEdit::to_json`]).
pub fn stage_json_for(working: &VersionEdit, perspective_mode: bool) -> String {
    VersionEdit {
        // `crop: undefined` drops the key, even a `null` one.
        crop: Field::Absent,
        perspective: if perspective_mode { Field::Absent } else { working.perspective.clone() },
        ..working.clone()
    }
    .to_json()
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/stageJson.test.ts (3 cases) ---
    use super::*;
    use crate::editing::{parse_edit, Perspective};
    use serde_json::{json, Value};

    fn working() -> VersionEdit {
        parse_edit(Some(
            &json!({
                "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "1:1"},
                "straighten": 2,
                "perspective": {"tl": [0, 0], "tr": [1, 0], "br": [1, 1], "bl": [0, 1]},
                "tone": {"ev": 0.5},
            })
            .to_string(),
        ))
    }

    fn parsed(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn drops_the_crop_and_keeps_everything_else() {
        let w = working();
        let r = parsed(&stage_json_for(&w, false));
        assert!(r.get("crop").is_none());
        assert_eq!(r["straighten"], json!(2));
        assert_eq!(serde_json::from_value::<Perspective>(r["perspective"].clone()).unwrap(), w.perspective.into_value().unwrap());
        assert_eq!(r["tone"]["ev"], json!(0.5));
    }

    #[test]
    fn drops_the_perspective_only_while_the_handles_are_up() {
        assert!(parsed(&stage_json_for(&working(), true)).get("perspective").is_none());
        assert!(parsed(&stage_json_for(&working(), false)).get("perspective").is_some());
    }

    /// `crop: undefined` drops even a `null` crop; a `null` perspective is carried when the
    /// handles are down (review finding, #103).
    #[test]
    fn explicit_null_crop_is_dropped_and_a_null_perspective_carried() {
        let w = parse_edit(Some(r#"{"crop":null,"perspective":null,"fade":null}"#));
        assert_eq!(parsed(&stage_json_for(&w, false)), json!({"fade": null, "perspective": null}));
        assert_eq!(parsed(&stage_json_for(&w, true)), json!({"fade": null}));
    }

    #[test]
    fn is_a_pure_function_of_its_inputs() {
        let w = working();
        assert_eq!(stage_json_for(&w, false), stage_json_for(&w.clone(), false));
        assert!(w.crop.is_set());
    }
}
