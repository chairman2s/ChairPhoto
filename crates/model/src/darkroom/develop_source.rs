//! The Darkroom's source badge and source state — a port of
//! `src/components/darkroom/developSource.ts` (docs/plans/raw-foundation). Pure: the view
//! feeds it core's [`DevelopSource`] from `develop_open` / the `develop:source` event.
//!
//! Core's `f32` numbers are widened the way the TS app saw them — through their shortest
//! decimal ([`js_compat::f32_as_js`]) — so `66.45` MP still reads "66.5" and a `-1.6` EV
//! camera match stays `-1.6`.

use chairphoto_core::develop_source::{DevelopSource, LensInfo};

use crate::darkroom::kelvin::AsShotWb;
use crate::js_compat;

/// The badge's visual weight: `Raw` is the developed state, `Warn` an honest exception,
/// `Plain` a JPEG or the plain camera preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    Raw,
    Warn,
    Plain,
}

/// What the stage is rendering from, in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceBadge {
    /// Short text for the bar.
    pub label: String,
    /// Hover text with the detail (decoder version, the reason a RAW is unsupported).
    pub title: String,
    pub tone: BadgeTone,
}

fn badge(label: impl Into<String>, title: impl Into<String>, tone: BadgeTone) -> SourceBadge {
    SourceBadge { label: label.into(), title: title.into(), tone }
}

/// The badge for a source.
pub fn badge_for(s: &DevelopSource) -> SourceBadge {
    match s {
        DevelopSource::Preview { preparing: true } => badge(
            "camera preview · preparing full quality",
            "The RAW is being decoded; the stage swaps to it when ready.",
            BadgeTone::Warn,
        ),
        DevelopSource::Preview { preparing: false } => badge(
            "camera preview",
            "The camera's embedded preview: the RAW is not prepared for this photo.",
            BadgeTone::Plain,
        ),
        DevelopSource::Raw { camera, megapixels, bits, decoder, .. } => badge(
            format!("RAW · {bits}-bit · {} MP", format_megapixels(js_compat::f32_as_js(*megapixels))),
            format!("{camera} — decoded by LibRaw {decoder}"),
            BadgeTone::Raw,
        ),
        DevelopSource::Unsupported { camera, reason } => badge(
            match camera.as_deref() {
                Some(c) if !c.is_empty() => format!("camera preview · RAW not supported yet · {c}"),
                _ => "camera preview · RAW not supported yet".to_string(),
            },
            format!("The bundled decoder cannot open this file yet ({reason}). Develop works on the camera's preview."),
            BadgeTone::Warn,
        ),
        DevelopSource::Jpeg => {
            badge("JPEG · 8-bit", "Not a RAW: the file's own pixels are its full quality.", BadgeTone::Plain)
        }
        DevelopSource::NoDecoder => badge(
            "camera preview · no RAW decoder in this build",
            "This build was compiled without the `raw` feature.",
            BadgeTone::Warn,
        ),
    }
}

/// One decimal, trailing zero dropped: 66.45 → "66.5", 33.0 → "33", 9.62 → "9.6"
/// (`(Math.round(mp * 10) / 10).toString()`).
pub fn format_megapixels(mp: f64) -> String {
    js_compat::number_to_string(js_compat::round(mp * 10.0) / 10.0)
}

/// Whether the RAW is still being prepared — the engine a save would be stamped with is not
/// decided yet, so the Darkroom's autosave waits (explicit saves do not).
pub fn is_preparing(s: Option<&DevelopSource>) -> bool {
    matches!(s, Some(DevelopSource::Preview { preparing: true }))
}

/// What the stage renders from, reduced from the source events for one photo.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceState {
    /// The token to put in render requests — `None` = the camera preview.
    pub token: Option<String>,
    /// The engine the working record should be written for: 2 once the RAW is resident.
    pub engine: u32,
    /// The resident RAW's camera match, in EV (0 when unmeasured or not on the RAW).
    pub camera_ev: f64,
    /// The resident RAW's as-shot light, for Kelvin white balance.
    pub as_shot_wb: Option<AsShotWb>,
    /// The resident RAW's lens tables (none off the RAW or when the camera wrote none).
    pub lens: Option<LensInfo>,
    /// The latest source, for the badge.
    pub source: Option<DevelopSource>,
}

impl Default for SourceState {
    /// `INITIAL_SOURCE`: the camera preview, engine 1, nothing known yet.
    fn default() -> Self {
        SourceState { token: None, engine: 1, camera_ev: 0.0, as_shot_wb: None, lens: None, source: None }
    }
}

/// Fold a source event in. Events for another photo (`event_photo_id` set and different)
/// return `prev` unchanged; a resident RAW (a non-empty token) yields its token and engine
/// 2; anything else drops back to the preview path.
pub fn reduce_source(prev: SourceState, e: &DevelopSource, event_photo_id: Option<i64>, photo_id: i64) -> SourceState {
    if event_photo_id.is_some_and(|id| id != photo_id) {
        return prev;
    }
    if let DevelopSource::Raw { token: Some(token), camera_ev, as_shot_wb, lens, .. } = e {
        if !token.is_empty() {
            return SourceState {
                token: Some(token.clone()),
                engine: 2,
                camera_ev: camera_ev.map(js_compat::f32_as_js).unwrap_or(0.0),
                as_shot_wb: as_shot_wb.map(|[k, t]| AsShotWb { kelvin: js_compat::f32_as_js(k), tint: js_compat::f32_as_js(t) }),
                lens: lens.clone(),
                source: Some(e.clone()),
            };
        }
    }
    SourceState { source: Some(e.clone()), ..SourceState::default() }
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/developSource.test.ts (12 cases) ---
    use super::*;

    fn raw_with(f: impl FnOnce(&mut DevelopSource)) -> DevelopSource {
        let mut s = DevelopSource::Raw {
            camera: "Sony".into(),
            megapixels: 66.5,
            bits: 16,
            decoder: "x".into(),
            token: Some("w:5:3".into()),
            camera_ev: None,
            as_shot_wb: None,
            lens: None,
        };
        f(&mut s);
        s
    }

    fn raw() -> DevelopSource {
        raw_with(|_| {})
    }

    fn preview(preparing: bool) -> DevelopSource {
        DevelopSource::Preview { preparing }
    }

    fn unsupported(camera: Option<&str>, reason: &str) -> DevelopSource {
        DevelopSource::Unsupported { camera: camera.map(Into::into), reason: reason.into() }
    }

    fn sony_lens() -> LensInfo {
        LensInfo { source: "Sony built-in".into(), vignetting: true, distortion: true, chromatic: true }
    }

    #[test]
    fn names_a_supported_raw_by_depth_and_size() {
        let b = badge_for(&DevelopSource::Raw {
            camera: "Sony ILCE-7RM6".into(),
            megapixels: 66.45,
            bits: 16,
            decoder: "0.22.0-Devel".into(),
            token: None,
            camera_ev: None,
            as_shot_wb: None,
            lens: None,
        });
        assert_eq!(b.label, "RAW · 16-bit · 66.5 MP");
        assert!(b.title.contains("Sony ILCE-7RM6"));
        assert_eq!(b.tone, BadgeTone::Raw);
    }

    #[test]
    fn is_honest_about_an_unsupported_camera_naming_it_when_known() {
        let b = badge_for(&unsupported(Some("ILCE-7RM6"), "Unsupported file format"));
        assert_eq!(b.label, "camera preview · RAW not supported yet · ILCE-7RM6");
        assert!(b.title.contains("Unsupported file format"));
        assert_eq!(b.tone, BadgeTone::Warn);
        assert_eq!(badge_for(&unsupported(None, "x")).label, "camera preview · RAW not supported yet");
    }

    #[test]
    fn treats_a_jpeg_as_its_own_full_quality_and_a_decoder_less_build_as_a_warning() {
        let j = badge_for(&DevelopSource::Jpeg);
        assert_eq!((j.label.as_str(), j.tone), ("JPEG · 8-bit", BadgeTone::Plain));
        assert_eq!(badge_for(&DevelopSource::NoDecoder).tone, BadgeTone::Warn);
    }

    #[test]
    fn format_megapixels_keeps_one_decimal_and_drops_a_trailing_zero() {
        assert_eq!(format_megapixels(66.45), "66.5");
        assert_eq!(format_megapixels(33.0), "33");
        assert_eq!(format_megapixels(9.62), "9.6");
    }

    #[test]
    fn starts_on_the_preview_and_moves_to_the_raw_token_when_it_becomes_resident() {
        let preparing = reduce_source(SourceState::default(), &preview(true), Some(5), 5);
        assert_eq!(preparing.token, None);
        assert_eq!(preparing.engine, 1);
        let resident = reduce_source(preparing, &raw(), Some(5), 5);
        assert_eq!(resident.token.as_deref(), Some("w:5:3"));
        assert_eq!(resident.engine, 2);
    }

    #[test]
    fn ignores_events_for_another_photo() {
        assert_eq!(reduce_source(SourceState::default(), &raw(), Some(6), 5), SourceState::default());
    }

    #[test]
    fn drops_back_to_the_preview_when_the_source_is_not_a_resident_raw() {
        let resident = reduce_source(SourceState::default(), &raw(), Some(5), 5);
        let gone = reduce_source(resident, &unsupported(None, "x"), Some(5), 5);
        assert_eq!(gone.token, None);
        assert_eq!(gone.engine, 1);
        assert_eq!(gone.camera_ev, 0.0);
    }

    #[test]
    fn carries_the_as_shot_light_for_kelvin_and_none_off_the_raw() {
        let with_wb = raw_with(|s| {
            if let DevelopSource::Raw { as_shot_wb, .. } = s {
                *as_shot_wb = Some([5313.0, 2.4]);
            }
        });
        assert_eq!(reduce_source(SourceState::default(), &with_wb, Some(5), 5).as_shot_wb, Some(AsShotWb { kelvin: 5313.0, tint: 2.4 }));
        assert_eq!(reduce_source(SourceState::default(), &raw(), Some(5), 5).as_shot_wb, None);
        assert_eq!(reduce_source(SourceState::default(), &DevelopSource::Jpeg, Some(5), 5).as_shot_wb, None);
    }

    #[test]
    fn carries_the_resident_raws_lens_tables_and_none_off_the_raw() {
        let with_lens = raw_with(|s| {
            if let DevelopSource::Raw { lens, .. } = s {
                *lens = Some(sony_lens());
            }
        });
        assert_eq!(reduce_source(SourceState::default(), &with_lens, Some(5), 5).lens, Some(sony_lens()));
        assert_eq!(reduce_source(SourceState::default(), &raw(), Some(5), 5).lens, None);
        let resident = reduce_source(SourceState::default(), &with_lens, Some(5), 5);
        assert_eq!(reduce_source(resident, &preview(true), Some(5), 5).lens, None);
    }

    #[test]
    fn carries_the_resident_raws_camera_match_and_0_when_it_was_not_measured() {
        let with_ev = raw_with(|s| {
            if let DevelopSource::Raw { camera_ev, .. } = s {
                *camera_ev = Some(-1.6);
            }
        });
        // -1.6f32 widened as the TS app saw it (through JSON), not as `-1.600000023841858`.
        assert_eq!(reduce_source(SourceState::default(), &with_ev, Some(5), 5).camera_ev, -1.6);
        assert_eq!(reduce_source(SourceState::default(), &raw(), Some(5), 5).camera_ev, 0.0);
    }

    #[test]
    fn autosave_waits_only_while_the_raw_is_being_prepared() {
        assert!(is_preparing(Some(&preview(true))));
        assert!(!is_preparing(Some(&preview(false))));
        assert!(!is_preparing(None));
        assert!(!is_preparing(Some(&DevelopSource::Jpeg)));
        assert!(!is_preparing(Some(&unsupported(None, "x"))));
    }

    #[test]
    fn badges_the_preparing_state_honestly() {
        assert!(badge_for(&preview(true)).label.contains("preparing"));
        assert_eq!(badge_for(&preview(false)).tone, BadgeTone::Plain);
    }
}
