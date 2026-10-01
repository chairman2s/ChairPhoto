//! The Darkroom's framing maths — the record transitions of `GeometryRail` and the crop box,
//! perspective quad and level line of `EditStage` (`src/components/EditControls.tsx`), and the
//! geometry half of the bridge in `DarkroomView.tsx` (`applyAspect`, `applyStraighten`,
//! `startPerspective`, `clearPerspective`, `orientedDims`, `cropPx`), as pure functions over
//! [`VersionEdit`]. Coordinates are fractions (0–1) of the frame the stage shows: the record
//! rendered without its crop (the crop is an overlay) — see `stage_json`.

use crate::editing::{
    default_quad, fit_crop, inscribed_crop, level_from_line, Crop, Field, Perspective, QuadCorner, VersionEdit, ASPECTS,
};
use crate::js_compat;
use serde_json::Value;

/// The persisted overlay preference (`editor.crop_overlay`, `OVERLAY_KEY`).
pub const OVERLAY_KEY: &str = "editor.crop_overlay";

/// A crop box must stay at least this big, as a fraction of each side.
pub const MIN_CROP: f64 = 0.05;

/// A level line shorter than this (in frame pixels) is a click, not a line.
pub const LEVEL_MIN_PX: f64 = 8.0;

/// Below this many degrees the straighten is "none" (the key is dropped).
pub const STRAIGHTEN_EPSILON: f64 = 0.05;

fn clamp01(v: f64) -> f64 {
    js_compat::min(js_compat::max(v, 0.0), 1.0)
}

/// A corner of the crop box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CropCorner {
    Nw,
    Ne,
    Sw,
    Se,
}

pub const CROP_CORNERS: [CropCorner; 4] = [CropCorner::Nw, CropCorner::Ne, CropCorner::Sw, CropCorner::Se];

impl CropCorner {
    pub fn key(self) -> &'static str {
        match self {
            CropCorner::Nw => "nw",
            CropCorner::Ne => "ne",
            CropCorner::Sw => "sw",
            CropCorner::Se => "se",
        }
    }

    /// Where the corner sits on `crop`, as fractions.
    pub fn position(self, crop: &Crop) -> (f64, f64) {
        let x = if matches!(self, CropCorner::Nw | CropCorner::Sw) { crop.x } else { crop.x + crop.w };
        let y = if matches!(self, CropCorner::Nw | CropCorner::Ne) { crop.y } else { crop.y + crop.h };
        (x, y)
    }

    /// The opposite corner — what stays put while this one is dragged.
    pub fn anchor(self, crop: &Crop) -> (f64, f64) {
        let ax = if matches!(self, CropCorner::Nw | CropCorner::Sw) { crop.x + crop.w } else { crop.x };
        let ay = if matches!(self, CropCorner::Nw | CropCorner::Ne) { crop.y + crop.h } else { crop.y };
        (ax, ay)
    }
}

/// The record's crop when it is a usable rectangle (a `null` or raw one is not drawn).
pub fn crop_of(working: &VersionEdit) -> Option<&Crop> {
    working.crop.value()
}

/// The record's perspective quad when it is a usable one.
pub fn perspective_of(working: &VersionEdit) -> Option<&Perspective> {
    working.perspective.value()
}

/// The aspect chip that is on: `crop?.aspect ?? "Original"`.
pub fn aspect_label(working: &VersionEdit) -> String {
    match &working.crop {
        Field::Set(c) => c.aspect.value().cloned().unwrap_or_else(|| "Original".into()),
        Field::Raw(Value::Object(o)) => match o.get("aspect") {
            Some(Value::String(s)) => s.clone(),
            _ => "Original".into(),
        },
        _ => "Original".into(),
    }
}

/// A preset's locked pixel aspect (width / height); `None` for "Original" and "Free".
pub fn ratio_for(label: &str) -> Option<f64> {
    if label == "Original" || label == "Free" {
        return None;
    }
    ASPECTS.iter().find(|a| a.label == label).and_then(|a| a.ratio)
}

/// An aspect chip (`applyAspect`): "Original" drops the crop; "Free" unlocks the current
/// crop (or starts a centred 80 % one); a ratio fits the largest centred crop of that aspect
/// in `src` (oriented pixel dimensions). `None` when a ratio needs dimensions not known yet.
pub fn apply_aspect(working: &VersionEdit, label: &str, src: Option<(f64, f64)>) -> Option<VersionEdit> {
    let crop = match label {
        "Original" => Field::Absent,
        "Free" => match &working.crop {
            Field::Set(c) => Field::Set(Crop { aspect: Field::Set("Free".into()), ..c.clone() }),
            // `{ ...w.crop, aspect: "Free" }` over a raw object keeps what it holds.
            Field::Raw(Value::Object(o)) => {
                let mut o = o.clone();
                o.insert("aspect".into(), Value::String("Free".into()));
                Field::Raw(Value::Object(o))
            }
            _ => Field::Set(Crop { aspect: Field::Set("Free".into()), ..Crop::rect(0.1, 0.1, 0.8, 0.8) }),
        },
        _ => {
            let ratio = ratio_for(label)?;
            let (w, h) = src?;
            Field::Set(Crop { aspect: Field::Set(label.into()), ..fit_crop(w, h, ratio, 1.0) })
        }
    };
    Some(VersionEdit { crop, ..working.clone() })
}

/// The straighten angle (`working.straighten ?? 0`, a raw one coerced).
pub fn straighten_of(working: &VersionEdit) -> f64 {
    working.straighten.num_or(0.0)
}

/// Straighten to `deg` (`applyStraighten`), clamped to ±45°. The crop auto-insets so the
/// rotation's corners stay hidden (with `src` known; otherwise the crop is kept); back near
/// 0° both keys go.
pub fn apply_straighten(working: &VersionEdit, deg: f64, src: Option<(f64, f64)>) -> VersionEdit {
    let d = crate::editing::clamp_straighten(deg);
    let none = d.abs() < STRAIGHTEN_EPSILON;
    VersionEdit {
        straighten: if none { Field::Absent } else { Field::Set(d) },
        crop: if none {
            Field::Absent
        } else {
            match src {
                Some((w, h)) => Field::Set(inscribed_crop(w, h, d)),
                None => working.crop.clone(),
            }
        },
        ..working.clone()
    }
}

/// A level line drawn from `(x1, y1)` to `(x2, y2)` in frame pixels: the rotation to add, or
/// `None` for a line too short to mean anything.
pub fn level_delta(x1: f64, y1: f64, x2: f64, y2: f64) -> Option<f64> {
    ((x2 - x1).hypot(y2 - y1) > LEVEL_MIN_PX).then(|| level_from_line(x1, y1, x2, y2))
}

/// "Correct perspective" (`startPerspective`): the quad the record has, else the default one;
/// the crop goes (it was framed on the unsquared picture).
pub fn start_perspective(working: &VersionEdit) -> VersionEdit {
    let perspective = if working.perspective.is_truthy() { working.perspective.clone() } else { Field::Set(default_quad()) };
    VersionEdit { perspective, crop: Field::Absent, ..working.clone() }
}

/// Perspective "Reset" (`clearPerspective`): quad and crop both go.
pub fn clear_perspective(working: &VersionEdit) -> VersionEdit {
    VersionEdit { perspective: Field::Absent, crop: Field::Absent, ..working.clone() }
}

/// The record with `crop` (a drag of the box).
pub fn with_crop(working: &VersionEdit, crop: Crop) -> VersionEdit {
    VersionEdit { crop: Field::Set(crop), ..working.clone() }
}

/// Dragging the crop body by `(dx, dy)` (fractions of the frame) from where it started:
/// it moves, staying inside the frame. Size, aspect and unknown keys stay.
pub fn moved_crop(start: &Crop, dx: f64, dy: f64) -> Crop {
    Crop {
        x: js_compat::min(js_compat::max(start.x + dx, 0.0), 1.0 - start.w),
        y: js_compat::min(js_compat::max(start.y + dy, 0.0), 1.0 - start.h),
        ..start.clone()
    }
}

/// Dragging a corner to `(mx, my)` (fractions of the frame, clamped) with the opposite
/// corner `anchor` fixed. With `ratio` (pixel width / height, in the `dims` frame) the box
/// keeps that aspect, driven by the axis that moved more and scaled to fit toward the drag;
/// free, each edge is clamped to the frame. Never below [`MIN_CROP`]. Aspect and unknown
/// keys of `crop` stay.
pub fn resized_crop(crop: &Crop, anchor: (f64, f64), mx: f64, my: f64, ratio: Option<f64>, dims: Option<(f64, f64)>) -> Crop {
    let (ax, ay) = anchor;
    let (dw, dh) = dims.unwrap_or((1.0, 1.0));
    let (mx, my) = (clamp01(mx), clamp01(my));
    let (dx, dy) = (mx - ax, my - ay);
    let (mut w, mut h);
    match ratio.filter(|r| *r != 0.0) {
        Some(ratio) => {
            let wpx = dx.abs() * dw;
            let hpx = dy.abs() * dh;
            let hpx_or = if hpx == 0.0 { 1e-9 } else { hpx };
            if wpx / hpx_or > ratio {
                w = dx.abs();
                h = (w * dw) / (ratio * dh);
            } else {
                h = dy.abs();
                w = (h * dh * ratio) / dw;
            }
            let avail_w = if dx >= 0.0 { 1.0 - ax } else { ax };
            let avail_h = if dy >= 0.0 { 1.0 - ay } else { ay };
            let or = |v: f64| if v == 0.0 { 1e-9 } else { v };
            let s = js_compat::min(1.0, js_compat::min(avail_w / or(w), avail_h / or(h)));
            w *= s;
            h *= s;
        }
        None => {
            w = dx.abs();
            h = dy.abs();
        }
    }
    w = js_compat::max(w, MIN_CROP);
    h = js_compat::max(h, MIN_CROP);
    let mut x = if dx >= 0.0 { ax } else { ax - w };
    let mut y = if dy >= 0.0 { ay } else { ay - h };
    if ratio.is_none() {
        if x < 0.0 {
            w += x;
            x = 0.0;
        }
        if y < 0.0 {
            h += y;
            y = 0.0;
        }
        if x + w > 1.0 {
            w = 1.0 - x;
        }
        if y + h > 1.0 {
            h = 1.0 - y;
        }
    }
    Crop { x, y, w, h, ..crop.clone() }
}

/// Dragging one corner of the perspective quad to `(x, y)` (fractions, clamped).
pub fn with_quad_corner(working: &VersionEdit, corner: QuadCorner, x: f64, y: f64) -> VersionEdit {
    let Some(p) = perspective_of(working) else { return working.clone() };
    let mut p = p.clone();
    *p.corner_mut(corner) = [clamp01(x), clamp01(y)];
    VersionEdit { perspective: Field::Set(p), ..working.clone() }
}

/// The original's pixel dimensions as the stage shows them (`orientedDims`): the sensor's
/// `photo` dimensions are unrotated, the frame `shown` is oriented — swapped to match.
pub fn oriented_dims(photo: Option<(f64, f64)>, shown: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let (pw, ph) = photo.filter(|(w, h)| *w > 0.0 && *h > 0.0)?;
    match shown {
        Some((sw, sh)) if (sh > sw) != (ph > pw) => Some((ph, pw)),
        _ => Some((pw, ph)),
    }
}

/// The output size of the crop in original pixels (`cropPx`), the full frame without one.
pub fn crop_px(working: &VersionEdit, oriented: Option<(f64, f64)>) -> Option<(i64, i64)> {
    let (w, h) = oriented?;
    let (cw, ch) = crop_of(working).map_or((1.0, 1.0), |c| (c.w, c.h));
    Some((js_compat::round(cw * w) as i64, js_compat::round(ch * h) as i64))
}

/// "Lens": the camera's lens-correction switch (`{ builtin: true }`, or the key dropped).
pub fn set_lens(working: &VersionEdit, on: bool) -> VersionEdit {
    VersionEdit {
        lens: if on { Field::Set(crate::editing::Lens { builtin: true, ..Default::default() }) } else { Field::Absent },
        ..working.clone()
    }
}

/// Whether the lens correction is on (`!!working.lens?.builtin`).
pub fn lens_on(working: &VersionEdit) -> bool {
    working.lens.value().is_some_and(|l| l.builtin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editing::parse_edit;
    use serde_json::json;

    fn rec(v: Value) -> VersionEdit {
        parse_edit(Some(&v.to_string()))
    }

    fn js(e: &VersionEdit) -> Value {
        serde_json::from_str(&e.to_json()).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn aspect_chips_drop_unlock_or_fit_the_crop() {
        let w = rec(json!({"tone": {"ev": 0.5}, "crop": {"x": 0.2, "y": 0.2, "w": 0.5, "h": 0.5, "aspect": "1:1", "k": 1}}));
        assert_eq!(aspect_label(&w), "1:1");
        assert_eq!(js(&apply_aspect(&w, "Original", None).unwrap()), json!({"tone": {"ev": 0.5}}));
        let free = apply_aspect(&w, "Free", None).unwrap();
        assert_eq!(js(&free)["crop"], json!({"x": 0.2, "y": 0.2, "w": 0.5, "h": 0.5, "aspect": "Free", "k": 1}), "unknown keys stay");
        assert_eq!(js(&apply_aspect(&VersionEdit::default(), "Free", None).unwrap())["crop"], json!({"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "Free"}));
        assert!(apply_aspect(&w, "4:5", None).is_none(), "a ratio needs the frame's size");
        let c = apply_aspect(&w, "1:1", Some((6000.0, 4000.0))).unwrap();
        let c = crop_of(&c).unwrap();
        assert!(close(c.w * 6000.0, c.h * 4000.0), "square in pixels");
        assert_eq!(c.aspect.as_deref(), Some("1:1"));
        assert_eq!(aspect_label(&VersionEdit::default()), "Original");
        assert_eq!(ratio_for("Free"), None);
        assert_eq!(ratio_for("16:9"), Some(16.0 / 9.0));
    }

    #[test]
    fn straightening_insets_the_crop_and_zero_clears_both() {
        let w = rec(json!({"crop": {"x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5}}));
        let s = apply_straighten(&w, 3.0, Some((6000.0, 4000.0)));
        assert_eq!(s.straighten.get(), Some(3.0));
        let c = crop_of(&s).unwrap();
        assert!(c.w < 1.0 && close(c.x, (1.0 - c.w) / 2.0), "inscribed and centred");
        assert_eq!(apply_straighten(&w, 99.0, None).straighten.get(), Some(45.0), "clamped; the crop kept unmeasured");
        assert_eq!(apply_straighten(&w, 99.0, None).crop, w.crop);
        assert_eq!(js(&apply_straighten(&s, 0.01, Some((6000.0, 4000.0)))), json!({}));
        assert_eq!(level_delta(0.0, 0.0, 5.0, 5.0), None, "a click is not a line");
        assert!(close(level_delta(0.0, 0.0, 100.0, 10.0).unwrap(), -(10f64.atan2(100.0).to_degrees())));
    }

    #[test]
    fn perspective_starts_from_the_records_quad_and_resets_with_the_crop() {
        let w = rec(json!({"crop": {"x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5}, "fade": 0.2}));
        let p = start_perspective(&w);
        assert_eq!(js(&p), json!({"perspective": {"tl": [0.06, 0.06], "tr": [0.94, 0.06], "br": [0.94, 0.94], "bl": [0.06, 0.94]}, "fade": 0.2}));
        let moved = with_quad_corner(&p, QuadCorner::Tr, 1.2, 0.1);
        assert_eq!(js(&moved)["perspective"]["tr"], json!([1, 0.1]), "clamped to the frame");
        assert_eq!(js(&start_perspective(&moved))["perspective"]["tr"], json!([1, 0.1]), "an existing quad is kept");
        assert_eq!(js(&clear_perspective(&moved)), json!({"fade": 0.2}));
        assert_eq!(with_quad_corner(&w, QuadCorner::Tl, 0.5, 0.5), w, "no quad, nothing to drag");
    }

    #[test]
    fn the_crop_box_moves_inside_the_frame() {
        let c = Crop { aspect: Field::Set("4:5".into()), ..Crop::rect(0.2, 0.2, 0.5, 0.4) };
        let m = moved_crop(&c, 0.6, -0.5);
        assert!(close(m.x, 0.5) && close(m.y, 0.0), "{m:?}");
        assert_eq!((m.w, m.h, m.aspect.clone()), (0.5, 0.4, c.aspect.clone()));
    }

    #[test]
    fn a_free_corner_drag_resizes_and_clamps_each_edge() {
        let c = Crop::rect(0.2, 0.2, 0.5, 0.5);
        let anchor = CropCorner::Se.anchor(&c);
        assert_eq!(anchor, (0.2, 0.2));
        let r = resized_crop(&c, anchor, 0.9, 0.6, None, None);
        assert!(close(r.w, 0.7) && close(r.h, 0.4) && close(r.x, 0.2));
        // Past the anchor: the box flips to the other side.
        let r = resized_crop(&c, anchor, 0.1, 0.1, None, None);
        assert!(close(r.x, 0.1) && close(r.w, 0.1) && close(r.y, 0.1));
        // Never below the minimum.
        let r = resized_crop(&c, anchor, 0.21, 0.21, None, None);
        assert!(close(r.w, MIN_CROP) && close(r.h, MIN_CROP));
    }

    #[test]
    fn a_locked_corner_drag_keeps_the_pixel_aspect_and_fits() {
        let c = Crop::rect(0.0, 0.0, 0.5, 0.5);
        let dims = Some((6000.0, 4000.0));
        let anchor = CropCorner::Se.anchor(&c);
        let r = resized_crop(&c, anchor, 0.6, 0.2, Some(1.0), dims);
        assert!(close(r.w * 6000.0, r.h * 4000.0), "square in pixels: {r:?}");
        // Dragged far: scaled to fit the room toward the drag, still square.
        let r = resized_crop(&c, anchor, 1.0, 1.0, Some(1.0), dims);
        assert!(r.x + r.w <= 1.0 + 1e-9 && r.y + r.h <= 1.0 + 1e-9);
        assert!(close(r.w * 6000.0, r.h * 4000.0));
    }

    #[test]
    fn oriented_dims_and_the_output_size() {
        assert_eq!(oriented_dims(Some((6000.0, 4000.0)), Some((400.0, 600.0))), Some((4000.0, 6000.0)), "a portrait frame swaps");
        assert_eq!(oriented_dims(Some((6000.0, 4000.0)), Some((600.0, 400.0))), Some((6000.0, 4000.0)));
        assert_eq!(oriented_dims(None, Some((600.0, 400.0))), None);
        let w = rec(json!({"crop": {"x": 0, "y": 0, "w": 0.5, "h": 0.25}}));
        assert_eq!(crop_px(&w, Some((6000.0, 4000.0))), Some((3000, 1000)));
        assert_eq!(crop_px(&VersionEdit::default(), Some((6000.0, 4000.0))), Some((6000, 4000)));
    }

    #[test]
    fn the_lens_switch() {
        let w = rec(json!({"lens": {"builtin": true, "x": 1}}));
        assert!(lens_on(&w));
        assert_eq!(js(&set_lens(&w, false)), json!({}));
        assert_eq!(js(&set_lens(&VersionEdit::default(), true)), json!({"lens": {"builtin": true}}));
    }
}
