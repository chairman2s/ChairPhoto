//! The Faces module's pure parts (unit-tested): box geometry over the loupe's zoomable
//! image, the person picker's rows, the root type-ahead, the result lines, and the People
//! view's naming paths, avatar crops and review threshold.

use crate::loupe::zoom::ZoomView;
use crate::shell::style::Colors;
use chairphoto_core::app::faces::{AcceptPersonOutcome, FaceBboxJson};
use chairphoto_core::app::{FacesIndexDone, FacesMatchDone};
use chairphoto_core::catalog::Tag;
use gpui_kit::Hsla;

// --- geometry -------------------------------------------------------------------------------

/// A rectangle in the loupe container's own coordinates (logical pixels from its top-left).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// A normalized box `(x, y, w, h)` (top-left corner) in an image's frame, carried into the
/// frame of that image turned `degrees` clockwise — what `thumbnails::rotate_image` does to
/// the pixels (90: a point `(x, y)` lands at `(1 − y, x)`). Anything but 90/180/270 (after
/// normalising) is no turn.
///
/// **The canonical frame.** Face boxes are stored in the photo's EXIF-oriented frame without
/// its non-destructive `user_rotation`: the frame the indexer detects in (the unrotated
/// preview) and the one the MWG export writes. The loupe draws the picture with the user
/// rotation applied, so the overlay turns each box by it to draw it and turns a drawn box back
/// by it ([`unrotate_box`]) to store it.
pub fn rotate_box(b: (f32, f32, f32, f32), degrees: i64) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = b;
    match ((degrees % 360) + 360) % 360 {
        90 => (1. - y - h, x, h, w),
        180 => (1. - x - w, 1. - y - h, w, h),
        270 => (y, 1. - x - w, h, w),
        _ => b,
    }
}

/// [`rotate_box`]'s inverse: a box in the frame turned `degrees` clockwise, back in the
/// unturned (canonical) frame.
pub fn unrotate_box(b: (f32, f32, f32, f32), degrees: i64) -> (f32, f32, f32, f32) {
    rotate_box(b, -degrees)
}

/// Where a face box lands on screen. `bbox` is normalized 0–1 against the frame of the picture
/// the loupe draws (the caller turns a stored box into it with [`rotate_box`]); `natural` is the
/// size of the picture the loupe draws, `container` its container, and `view` its pan/zoom.
/// The image's own placement ([`ZoomView::placement`]) is the reference, so a box is drawn
/// through exactly the transform the picture is (React mapped through the parsed CSS transform).
pub fn bbox_to_screen(bbox: FaceBboxJson, natural: (f32, f32), container: (f32, f32), view: ZoomView) -> ScreenRect {
    let (l, t, w, h) = view.placement(natural, container);
    ScreenRect { left: l + bbox.x * w, top: t + bbox.y * h, width: bbox.w * w, height: bbox.h * h }
}

/// The inverse for one point: container coordinates → normalized image coordinates (outside
/// 0–1 in the letterbox).
pub fn screen_to_image(p: (f32, f32), natural: (f32, f32), container: (f32, f32), view: ZoomView) -> (f32, f32) {
    let (l, t, w, h) = view.placement(natural, container);
    if w <= 0. || h <= 0. {
        return (0., 0.);
    }
    ((p.0 - l) / w, (p.1 - t) / h)
}

/// A drag shorter than this (either side, logical px) is a click, not a box (React: 8 px).
pub const MIN_DRAG: f32 = 8.;

/// The normalized box `(x, y, w, h)` a drag from `a` to `b` (container coordinates) draws,
/// clamped to the image; `None` for an accidental click or a box entirely in the letterbox.
pub fn drag_to_bbox(
    a: (f32, f32),
    b: (f32, f32),
    natural: (f32, f32),
    container: (f32, f32),
    view: ZoomView,
) -> Option<(f64, f64, f64, f64)> {
    let (left, top) = (a.0.min(b.0), a.1.min(b.1));
    let (w, h) = ((a.0 - b.0).abs(), (a.1 - b.1).abs());
    if w < MIN_DRAG || h < MIN_DRAG {
        return None;
    }
    let p0 = screen_to_image((left, top), natural, container, view);
    let p1 = screen_to_image((left + w, top + h), natural, container, view);
    let x = p0.0.clamp(0., 1.) as f64;
    let y = p0.1.clamp(0., 1.) as f64;
    let bw = p1.0.clamp(0., 1.) as f64 - x;
    let bh = p1.1.clamp(0., 1.) as f64 - y;
    if bw <= chairphoto_core::app::faces::MIN_DRAWN || bh <= chairphoto_core::app::faces::MIN_DRAWN {
        return None;
    }
    Some((x, y, bw, bh))
}

// --- states ---------------------------------------------------------------------------------

/// A face's colour by state (`chipColor`).
pub fn state_color(state: &str, colors: Colors) -> Hsla {
    match state {
        "confirmed" => colors.ok,
        "suggested" => colors.rating,
        "rejected" => colors.danger,
        "ignored" => colors.mute,
        _ => colors.accent,
    }
}

/// The name a box's chip shows.
pub fn chip_name(person: Option<&str>, state: &str) -> String {
    match (person, state) {
        (Some(name), _) => name.to_string(),
        (None, "rejected") => "Rejected".into(),
        (None, "ignored") => "Ignored".into(),
        _ => "Unknown".into(),
    }
}

// --- the person picker ----------------------------------------------------------------------

/// One row of the person picker.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRow {
    Tag { id: i64, name: String, full_path: String },
    /// "＋ Create “name”" — offered when the typed name matches no tag exactly.
    Create(String),
}

/// The picker's rows for `query` (`PersonPicker`): tags whose path contains it (case-blind),
/// then "Create" when creating is allowed and the trimmed query names no tag exactly (leaf
/// name or full path).
pub fn picker_rows(tags: &[Tag], query: &str, can_create: bool) -> Vec<PickerRow> {
    let q = query.trim().to_lowercase();
    let mut rows: Vec<PickerRow> = tags
        .iter()
        .filter(|t| q.is_empty() || t.full_path.to_lowercase().contains(&q))
        .map(|t| PickerRow::Tag { id: t.id, name: t.name.clone(), full_path: t.full_path.clone() })
        .collect();
    let exact = tags.iter().any(|t| t.name.to_lowercase() == q || t.full_path.to_lowercase() == q);
    if can_create && !q.is_empty() && !exact {
        rows.push(PickerRow::Create(query.trim().to_string()));
    }
    rows
}

/// The people-root type-ahead (`rootSuggestions`): with text, every tag path containing it;
/// empty, the root-level tags. At most eight.
pub fn root_suggestions(tags: &[Tag], text: &str) -> Vec<String> {
    let q = text.trim().to_lowercase();
    tags.iter()
        .filter(|t| if q.is_empty() { !t.full_path.contains('/') } else { t.full_path.to_lowercase().contains(&q) })
        .map(|t| t.full_path.clone())
        .take(8)
        .collect()
}

/// Move a highlight by `delta` within `len` rows (clamped, as the React list did).
pub fn step_highlight(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

// --- result lines ---------------------------------------------------------------------------

/// How an index run ended, from its counters (`indexDoneMessage`).
pub fn index_done_message(d: &FacesIndexDone) -> String {
    if d.total == 0 {
        return "Indexing: nothing to do — all photos are already indexed.".into();
    }
    let mut skips = Vec::new();
    if d.offline > 0 {
        skips.push(format!("{} offline (connect the NAS and re-run)", d.offline));
    }
    if d.failed > 0 {
        skips.push(format!("{} unreadable", d.failed));
    }
    let skip_note = if skips.is_empty() { String::new() } else { format!(" Skipped: {} — still queued.", skips.join(", ")) };
    if d.aborted {
        return format!("Indexing cancelled at {} of {} photos.{skip_note}", d.done, d.total);
    }
    if d.done < d.total {
        return format!("Indexing finished: {} of {} photos processed.{skip_note}", d.done, d.total);
    }
    format!("Indexing complete: {} photo{} processed.", d.total, if d.total == 1 { "" } else { "s" })
}

/// What a batch confirm did (`batchConfirmMessage`): every selected photo is in exactly one
/// bucket, so "confirmed on 6 of 9" means what it says.
pub fn batch_confirm_message(out: &AcceptPersonOutcome, selected: usize, person: &str) -> String {
    let mut notes = Vec::new();
    if out.photos_already_confirmed > 0 {
        notes.push(format!("{} already confirmed", out.photos_already_confirmed));
    }
    if out.photos_without_suggestion > 0 {
        notes.push(format!("{} had no suggestion for {person}", out.photos_without_suggestion));
    }
    let tail = if notes.is_empty() { String::new() } else { format!(" — {}", notes.join(", ")) };
    if out.photos_confirmed == 0 {
        return format!("Nothing to confirm on the {selected} selected photos{tail}.");
    }
    let faces = if out.faces_confirmed != out.photos_confirmed { format!(" ({} faces)", out.faces_confirmed) } else { String::new() };
    format!("Confirmed {person} on {} of {selected} selected photos{faces}{tail}.", out.photos_confirmed)
}

/// The progress line (`Indexing: done / total (pct%)`).
pub fn progress_line(done: usize, total: usize) -> (String, Option<usize>) {
    let pct = (total > 0).then(|| ((done as f64 / total as f64) * 100.).round() as usize);
    let of = if total > 0 { total.to_string() } else { "…".into() };
    let line = match pct {
        Some(p) => format!("Indexing: {done} / {of} ({p}%)"),
        None => format!("Indexing: {done} / {of}"),
    };
    (line, pct)
}

/// The matching run's progress line: the phase, then `done / total (pct%)` once the phase has
/// a size.
pub fn match_progress_line(phase: &str, done: usize, total: usize) -> (String, Option<usize>) {
    let pct = (total > 0).then(|| ((done as f64 / total as f64) * 100.).round() as usize);
    let line = match pct {
        Some(p) => format!("Matching ({phase}): {done} / {total} ({p}%)"),
        None => format!("Matching ({phase})…"),
    };
    (line, pct)
}

/// The line a finished match leaves (`handleMatchDone` in faces.tsx): `Err` for a failure.
pub fn match_done_message(d: &FacesMatchDone) -> Result<String, String> {
    if let Some(e) = &d.error {
        return Err(format!("Matching failed: {e}"));
    }
    if d.aborted {
        return Ok("Matching cancelled — partial results were kept.".into());
    }
    let Some(o) = &d.outcome else {
        return Err("Matching finished without reporting what it did.".into());
    };
    let suggested = o.constrained + o.open;
    Ok(if o.seeded + suggested + o.clustered == 0 {
        if o.people == 0 {
            "Matching: no seeds found yet — check that the people root matches your person tags, and that indexing has \
             run."
                .into()
        } else {
            format!("Matching: nothing new to propose ({} known people).", o.people)
        }
    } else {
        format!(
            "Matching: {} seeded, {suggested} suggested, {} clustered ({} known people).",
            o.seeded, o.clustered, o.people
        )
    })
}

/// The person tag a name typed in the People view becomes (`NameClusterModal`): under the
/// people root, unless the text already is the root or a path below it.
pub fn named_path(root: &str, typed: &str) -> String {
    let (root, typed) = (root.trim(), typed.trim().trim_matches('/').trim());
    if root.is_empty() || typed == root || typed.starts_with(&format!("{root}/")) {
        typed.to_string()
    } else {
        format!("{root}/{typed}")
    }
}

/// The name type-ahead (`NameClusterModal`): up to 8 people-root tag paths containing the
/// text, case-insensitively; nothing for an empty text.
pub fn name_suggestions(tags: &[Tag], typed: &str) -> Vec<String> {
    let q = typed.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    tags.iter().filter(|t| t.full_path.to_lowercase().contains(&q)).take(8).map(|t| t.full_path.clone()).collect()
}

/// Where a face avatar draws its photo's thumbnail inside a `size`×`size` square: scaled
/// uniformly so the face's longer side fills the square, the face centred. `bbox` is the
/// face in the thumbnail's frame (turned by the user rotation, [`rotate_box`]); `natural` is
/// the thumbnail's pixel size. Returns `(left, top, width, height)` of the whole thumbnail.
/// (React stretched the image non-uniformly to the box.)
pub fn avatar_placement(bbox: (f32, f32, f32, f32), natural: (f32, f32), size: f32) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = bbox;
    let (nw, nh) = (natural.0.max(1.), natural.1.max(1.));
    let face = (w.max(0.01) * nw).max(h.max(0.01) * nh);
    let s = size / face;
    let (dw, dh) = (nw * s, nh * s);
    let (cx, cy) = ((x + w / 2.) * dw, (y + h / 2.) * dh);
    (size / 2. - cx, size / 2. - cy, dw, dh)
}

/// Whether a suggestion's `confidence` reaches the queue's `threshold`: the exact fraction,
/// as React's `e.confidence >= confidenceThreshold` — not the rounded percent the label shows,
/// so a 0.795 (shown as 80%) is not confirmed by "Confirm all ≥80%". The row dimming uses the
/// same test, so what is dimmed is exactly what "Confirm all" leaves out.
pub fn reaches(confidence: f64, threshold: f64) -> bool {
    confidence >= threshold
}

/// The suggestions at or above `threshold` (the queue's "Confirm all ≥ X%"), by index.
pub fn at_or_above(confidences: impl IntoIterator<Item = f64>, threshold: f64) -> Vec<usize> {
    confidences.into_iter().enumerate().filter(|&(_, c)| reaches(c, threshold)).map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_goes_under_the_people_root_unless_it_already_is() {
        assert_eq!(named_path("People", "Jane"), "People/Jane");
        assert_eq!(named_path("People", " People/Family/Jane "), "People/Family/Jane");
        assert_eq!(named_path("People", "People"), "People");
        assert_eq!(named_path("People", "Peoples/Jane"), "People/Peoples/Jane", "a prefix of the root is not under it");
        assert_eq!(named_path("", "Jane"), "Jane");
    }

    #[test]
    fn an_avatar_centres_the_face_and_keeps_the_aspect() {
        // A 400×200 thumbnail, a face 40×40 px at (100,50): scale 72/40 = 1.8.
        let (l, t, w, h) = avatar_placement((0.25, 0.25, 0.1, 0.2), (400., 200.), 72.);
        assert!((w - 720.).abs() < 1e-3 && (h - 360.).abs() < 1e-3, "uniform scale: {w}×{h}");
        // Face centre (120, 70) px × 1.8 = (216, 126) lands on the square's centre (36, 36).
        assert!((l - (36. - 216.)).abs() < 1e-3 && (t - (36. - 126.)).abs() < 1e-3, "{l},{t}");
    }

    /// The exact fraction, like React's `>=`: 0.795 is shown as 80% but is below 0.8, so
    /// "Confirm all ≥80%" leaves it (and dims its row); 0.8 itself is in.
    #[test]
    fn confirm_all_compares_the_exact_fraction() {
        assert_eq!(at_or_above([0.95, 0.8, 0.795, 0.7999999, 0.5], 0.8), vec![0, 1]);
        assert!(!reaches(0.795, 0.8) && reaches(0.8, 0.8));
        // A slider value (step 0.05, quantised as the view does) and an f32-stored confidence.
        let t = (0.8f32 as f64 * 20.).round() / 20.;
        assert!(reaches(0.8f32 as f64, t));
        assert_eq!(at_or_above([0.5], 0.0), vec![0]);
    }

    #[test]
    fn match_results_read_like_react() {
        use chairphoto_core::plugins::faces::MatchOutcome;
        let done = |outcome: Option<MatchOutcome>, aborted: bool, error: Option<&str>| FacesMatchDone {
            ok: error.is_none() && !aborted,
            outcome,
            aborted,
            job: 1,
            error: error.map(str::to_string),
        };
        let o = MatchOutcome { seeded: 1, constrained: 2, open: 3, clustered: 4, people: 5 };
        assert_eq!(
            match_done_message(&done(Some(o), false, None)).unwrap(),
            "Matching: 1 seeded, 5 suggested, 4 clustered (5 known people)."
        );
        assert!(match_done_message(&done(Some(MatchOutcome::default()), false, None)).unwrap().contains("no seeds"));
        let none = MatchOutcome { people: 2, ..Default::default() };
        assert_eq!(match_done_message(&done(Some(none), false, None)).unwrap(), "Matching: nothing new to propose (2 known people).");
        assert!(match_done_message(&done(None, true, None)).unwrap().contains("cancelled"));
        assert_eq!(match_done_message(&done(None, false, Some("boom"))).unwrap_err(), "Matching failed: boom");
        assert!(match_done_message(&done(None, false, None)).is_err());
        assert_eq!(match_progress_line("clustering unknowns", 5, 10).0, "Matching (clustering unknowns): 5 / 10 (50%)");
    }

    fn bb(x: f32, y: f32, w: f32, h: f32) -> FaceBboxJson {
        FaceBboxJson { x, y, w, h }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// At fit a landscape image letterboxes top and bottom; a box lands inside the image's
    /// own rectangle, scaled by the fit factor.
    #[test]
    fn a_box_at_fit_follows_the_letterbox() {
        // 400×200 in 800×800: drawn 800×400, 200 px bars above and below.
        let r = bbox_to_screen(bb(0.25, 0.5, 0.25, 0.25), (400., 200.), (800., 800.), ZoomView::FIT);
        assert_eq!(r, ScreenRect { left: 200., top: 200. + 200., width: 200., height: 100. });
    }

    /// A portrait (oriented) image letterboxes left and right: the box is against the
    /// oriented frame, never the sensor's.
    #[test]
    fn an_oriented_portrait_letterboxes_sideways() {
        // 200×400 in 800×800: drawn 400×800, 200 px bars left and right.
        let r = bbox_to_screen(bb(0.5, 0.25, 0.25, 0.125), (200., 400.), (800., 800.), ZoomView::FIT);
        assert_eq!(r, ScreenRect { left: 200. + 200., top: 200., width: 100., height: 100. });
    }

    /// Zoomed and panned, a box moves exactly as the image point under it does: the same
    /// transform as the picture (`ZoomView::placement`).
    #[test]
    fn a_box_follows_zoom_and_pan() {
        let (natural, container) = ((400., 200.), (800., 800.));
        let fit = bbox_to_screen(bb(0.25, 0.5, 0.25, 0.25), natural, container, ZoomView::FIT);
        let view = ZoomView { scale: 2., tx: 30., ty: -10. };
        let z = bbox_to_screen(bb(0.25, 0.5, 0.25, 0.25), natural, container, view);
        // About the container centre (400, 400): p' = c + (p − c)·s + t.
        assert!(close(z.left, 400. + (fit.left - 400.) * 2. + 30.), "{z:?}");
        assert!(close(z.top, 400. + (fit.top - 400.) * 2. - 10.), "{z:?}");
        assert!(close(z.width, fit.width * 2.) && close(z.height, fit.height * 2.));
    }

    #[test]
    fn screen_to_image_inverts_bbox_to_screen() {
        let (natural, container) = ((300., 500.), (640., 480.));
        let view = ZoomView { scale: 3.1, tx: -55., ty: 12. };
        let r = bbox_to_screen(bb(0.3, 0.6, 0.1, 0.1), natural, container, view);
        let (x, y) = screen_to_image((r.left, r.top), natural, container, view);
        assert!(close(x, 0.3) && close(y, 0.6), "{x} {y}");
    }

    /// A box turned with the photo lands on the pixels the turned picture has there: paint the
    /// box into a 40×20 canonical image, turn it with the core's own `rotate_image` (what the
    /// loupe's tiers get), and measure where the paint went. The inverse brings it back.
    #[test]
    fn a_turned_box_follows_the_turned_pixels() {
        use image::{DynamicImage, GenericImageView as _, Rgb, RgbImage};
        let (cw, ch) = (40u32, 20u32);
        let canonical = (0.1, 0.2, 0.3, 0.4); // x 4..16, y 4..12
        let mut img = RgbImage::new(cw, ch);
        for x in 4..16 {
            for y in 4..12 {
                img.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
        for degrees in [0i64, 90, 180, 270, -90, 450] {
            let turned = chairphoto_core::thumbnails::rotate_image(DynamicImage::ImageRgb8(img.clone()), degrees);
            let (w, h) = turned.dimensions();
            let painted: Vec<(u32, u32)> =
                turned.pixels().filter(|(_, _, p)| p.0[0] == 255).map(|(x, y, _)| (x, y)).collect();
            let (x0, x1) = (painted.iter().map(|p| p.0).min().unwrap(), painted.iter().map(|p| p.0).max().unwrap() + 1);
            let (y0, y1) = (painted.iter().map(|p| p.1).min().unwrap(), painted.iter().map(|p| p.1).max().unwrap() + 1);
            let want = (x0 as f32 / w as f32, y0 as f32 / h as f32, (x1 - x0) as f32 / w as f32, (y1 - y0) as f32 / h as f32);
            let got = rotate_box(canonical, degrees);
            let near = |a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)| {
                close(a.0, b.0) && close(a.1, b.1) && close(a.2, b.2) && close(a.3, b.3)
            };
            assert!(near(got, want), "{degrees}°: {got:?} vs the pixels' {want:?}");
            assert!(near(unrotate_box(got, degrees), canonical), "{degrees}°: back");
        }
    }

    /// A drawn drag: clicks and letterbox-only drags draw nothing; a drag past the image's
    /// edge is clamped to it.
    #[test]
    fn a_drag_draws_a_clamped_box() {
        let (natural, container) = ((400., 200.), (800., 800.));
        assert_eq!(drag_to_bbox((10., 10.), (15., 300.), natural, container, ZoomView::FIT), None, "a click");
        assert_eq!(drag_to_bbox((10., 10.), (300., 150.), natural, container, ZoomView::FIT), None, "all letterbox");
        let (x, y, w, h) = drag_to_bbox((600., 100.), (900., 300.), natural, container, ZoomView::FIT).unwrap();
        assert!((x - 0.75).abs() < 1e-6 && y == 0.0, "{x} {y}");
        assert!((w - 0.25).abs() < 1e-6 && (h - 0.25).abs() < 1e-6, "{w} {h}");
        // The same drag reversed draws the same box.
        assert_eq!(drag_to_bbox((900., 300.), (600., 100.), natural, container, ZoomView::FIT), Some((x, y, w, h)));
    }

    fn tag(id: i64, path: &str) -> Tag {
        Tag {
            id,
            uuid: String::new(),
            name: path.rsplit('/').next().unwrap().to_string(),
            full_path: path.to_string(),
            parent_id: None,
            description: String::new(),
            auto_rule: None,
            private: false,
        }
    }

    #[test]
    fn the_picker_filters_and_offers_create_only_for_new_names() {
        let tags = vec![tag(1, "People/Alice"), tag(2, "People/Bob"), tag(3, "People/Alicia")];
        assert_eq!(picker_rows(&tags, "", true).len(), 3, "empty: every tag, no Create");
        let rows = picker_rows(&tags, "ali", true);
        assert_eq!(rows.len(), 3, "two matches and Create: {rows:?}");
        assert_eq!(rows[2], PickerRow::Create("ali".into()));
        assert_eq!(picker_rows(&tags, " alice ", true).len(), 1, "an exact leaf name: no Create");
        assert_eq!(picker_rows(&tags, "people/bob", true).len(), 1, "an exact path: no Create");
        assert_eq!(picker_rows(&tags, "Zed", false), vec![], "no Create without creating");
        assert_eq!(step_highlight(0, -1, 3), 0);
        assert_eq!(step_highlight(2, 1, 3), 2);
        assert_eq!(step_highlight(1, 1, 3), 2);
        assert_eq!(step_highlight(5, 1, 0), 0);
    }

    #[test]
    fn the_root_typeahead_offers_top_level_tags_when_empty() {
        let tags = vec![tag(1, "People"), tag(2, "People/Alice"), tag(3, "Places"), tag(4, "Places/Oslo")];
        assert_eq!(root_suggestions(&tags, ""), vec!["People", "Places"]);
        assert_eq!(root_suggestions(&tags, "pe"), vec!["People", "People/Alice"]);
    }

    fn done(total: usize, done: usize, offline: usize, failed: usize, aborted: bool) -> FacesIndexDone {
        FacesIndexDone { ok: true, done, total, offline, failed, aborted, job: 1, error: None }
    }

    #[test]
    fn index_results_say_what_happened() {
        assert_eq!(index_done_message(&done(0, 0, 0, 0, false)), "Indexing: nothing to do — all photos are already indexed.");
        assert_eq!(index_done_message(&done(1, 1, 0, 0, false)), "Indexing complete: 1 photo processed.");
        assert_eq!(index_done_message(&done(9, 9, 0, 0, false)), "Indexing complete: 9 photos processed.");
        assert_eq!(
            index_done_message(&done(9, 6, 2, 1, false)),
            "Indexing finished: 6 of 9 photos processed. Skipped: 2 offline (connect the NAS and re-run), 1 unreadable — still queued."
        );
        assert_eq!(index_done_message(&done(9, 3, 0, 0, true)), "Indexing cancelled at 3 of 9 photos.");
        assert_eq!(progress_line(3, 0), ("Indexing: 3 / …".into(), None));
        assert_eq!(progress_line(1, 3), ("Indexing: 1 / 3 (33%)".into(), Some(33)));
    }

    #[test]
    fn batch_confirm_lines_count_every_bucket() {
        let out = AcceptPersonOutcome { photos_confirmed: 6, faces_confirmed: 7, photos_already_confirmed: 1, photos_without_suggestion: 2 };
        assert_eq!(
            batch_confirm_message(&out, 9, "Alice"),
            "Confirmed Alice on 6 of 9 selected photos (7 faces) — 1 already confirmed, 2 had no suggestion for Alice."
        );
        let none = AcceptPersonOutcome { photos_without_suggestion: 3, ..Default::default() };
        assert_eq!(batch_confirm_message(&none, 3, "Bob"), "Nothing to confirm on the 3 selected photos — 3 had no suggestion for Bob.");
    }
}
