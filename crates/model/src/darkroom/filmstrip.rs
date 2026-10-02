//! The Darkroom filmstrip's pure parts — a port of `src/components/darkroom/filmstrip.ts`:
//! which photos to render around the current one, and where a step lands. The strip follows
//! the Library's current order and filter.
//!
//! #134 adds what `Filmstrip.tsx` left to the DOM: where the strip scrolls so the current
//! frame sits in the middle (`scrollIntoView({ inline: "center" })`), and which look a frame
//! shows (the photo's cover token, which the thumbnail URL carried).

/// How many photos either side of the current one the strip renders.
pub const STRIP_RADIUS: usize = 40;

/// The slice of a list the strip renders: where it starts in the list, and its ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StripWindow {
    pub start: usize,
    pub ids: Vec<i64>,
}

/// The slice of `ids` the strip renders: up to `radius` either side of `current_id`; the
/// start of the list (`2 × radius + 1` ids) when the current photo is not in it.
pub fn window_around(ids: &[i64], current_id: i64, radius: usize) -> StripWindow {
    let Some(i) = ids.iter().position(|&id| id == current_id) else {
        return StripWindow { start: 0, ids: ids.iter().take(radius * 2 + 1).copied().collect() };
    };
    let start = i.saturating_sub(radius);
    let end = (i + radius + 1).min(ids.len());
    StripWindow { start, ids: ids[start..end].to_vec() }
}

/// The photo a step of `delta` lands on, or `None` at either end (no wrap-around) and when
/// the current photo is not in the list.
pub fn step_target(ids: &[i64], current_id: i64, delta: isize) -> Option<i64> {
    let i = ids.iter().position(|&id| id == current_id)?;
    let j = i.checked_add_signed(delta)?;
    ids.get(j).copied()
}

/// The strip as the view draws it: fixed-width frames in a row, `gap` apart, inside
/// `padding` on either side. The view lays the frames out with these numbers, so where a
/// frame sits follows from its index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripLayout {
    /// A frame's outer width (border included).
    pub frame: f32,
    pub gap: f32,
    /// The strip's horizontal padding, each side.
    pub padding: f32,
}

/// The GPUI strip: 72 px frames, 4 px apart (React's `gap: 4px`), 12 px padding.
pub const STRIP_LAYOUT: StripLayout = StripLayout { frame: 72.0, gap: 4.0, padding: 12.0 };

impl StripLayout {
    /// The left edge of frame `index`, from the strip's left edge when it is not scrolled.
    pub fn frame_left(&self, index: usize) -> f32 {
        self.padding + index as f32 * (self.frame + self.gap)
    }

    /// Everything the strip scrolls over for `count` frames, padding included.
    pub fn content_width(&self, count: usize) -> f32 {
        let gaps = count.saturating_sub(1) as f32 * self.gap;
        2.0 * self.padding + count as f32 * self.frame + gaps
    }

    /// How far to scroll so frame `index` of `count` is centred in a `viewport`-wide strip
    /// ([`centred_scroll`]).
    pub fn centre(&self, index: usize, count: usize, viewport: f32) -> f32 {
        centred_scroll(self.frame_left(index), self.frame, self.content_width(count), viewport)
    }
}

/// `scrollIntoView({ inline: "center" })` along one axis: the scroll position that puts an
/// item's centre on the viewport's centre, clamped to what the content allows — at the
/// start of the strip it stays at 0, at the end at `content − viewport`, and a strip that
/// fits does not scroll at all.
pub fn centred_scroll(item_left: f32, item_width: f32, content: f32, viewport: f32) -> f32 {
    let max = (content - viewport).max(0.0);
    (item_left + item_width / 2.0 - viewport / 2.0).clamp(0.0, max)
}

/// The look a frame shows: the photo's cover version and that cover's revision — bumped by
/// every change to the version's settings, a new cover, or the cover taken off — as the
/// row's cover token `"<version>:<rev>"` names it. `Filmstrip.tsx` put the token in the
/// thumbnail URL, so any change asked for the thumbnail again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoverLook {
    pub version: i64,
    pub rev: i64,
}

/// The look a row's cover token names; `None` (the plain thumbnail) for no token. The
/// catalog writes `"<version>:<rev>"` or nothing, so anything else is read as no cover.
pub fn cover_look(token: Option<&str>) -> Option<CoverLook> {
    let (version, rev) = token?.split_once(':')?;
    Some(CoverLook { version: version.parse().ok()?, rev: rev.parse().ok()? })
}

/// What has keyboard focus when an arrow key arrives. The TS version inspected the DOM
/// event target; the GPUI view says what it has focused instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyTarget {
    /// A text field or a slider (`<input>`, range sliders included).
    Input,
    /// A multi-line text field.
    TextArea,
    /// A drop-down.
    Select,
    /// Editable text that is not a field.
    ContentEditable,
    /// A button or nothing in particular.
    Other,
}

/// Whether a key event belongs to a control that uses arrow keys itself.
pub fn arrows_belong_to_target(t: Option<KeyTarget>) -> bool {
    matches!(t, Some(KeyTarget::Input | KeyTarget::TextArea | KeyTarget::Select | KeyTarget::ContentEditable))
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/filmstrip.test.ts (4 cases) ---
    use super::*;

    fn ids() -> Vec<i64> {
        (1..=100).collect()
    }

    #[test]
    fn window_around_centres_on_the_current_photo_clipped_at_the_ends() {
        let ids = ids();
        assert_eq!(window_around(&ids, 50, 3), StripWindow { start: 46, ids: vec![47, 48, 49, 50, 51, 52, 53] });
        assert_eq!(window_around(&ids, 1, 3), StripWindow { start: 0, ids: vec![1, 2, 3, 4] });
        assert_eq!(window_around(&ids, 100, 3), StripWindow { start: 96, ids: vec![97, 98, 99, 100] });
    }

    #[test]
    fn window_around_shows_the_start_of_the_list_when_the_current_photo_is_not_in_it() {
        assert_eq!(window_around(&ids(), 999, 2).ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn step_target_steps_within_the_list_and_stops_at_the_ends() {
        let ids = ids();
        assert_eq!(step_target(&ids, 50, 1), Some(51));
        assert_eq!(step_target(&ids, 50, -1), Some(49));
        assert_eq!(step_target(&ids, 100, 1), None);
        assert_eq!(step_target(&ids, 1, -1), None);
        assert_eq!(step_target(&ids, 999, 1), None);
    }

    #[test]
    fn arrows_belong_to_sliders_fields_and_selects() {
        assert!(arrows_belong_to_target(Some(KeyTarget::Input))); // range sliders included
        assert!(arrows_belong_to_target(Some(KeyTarget::TextArea)));
        assert!(arrows_belong_to_target(Some(KeyTarget::Select)));
        assert!(arrows_belong_to_target(Some(KeyTarget::ContentEditable)));
        assert!(!arrows_belong_to_target(Some(KeyTarget::Other)));
        assert!(!arrows_belong_to_target(None));
    }

    // --- #134: centring and cover looks (no TS cases: the DOM did both) ---

    #[test]
    fn centring_puts_the_frame_in_the_middle() {
        // A 400 px strip over 1000 px of content: a frame at 480..520 is centred at 300.
        assert_eq!(centred_scroll(480.0, 40.0, 1000.0, 400.0), 300.0);
    }

    #[test]
    fn centring_is_clamped_at_both_ends_and_a_strip_that_fits_does_not_scroll() {
        assert_eq!(centred_scroll(12.0, 72.0, 1000.0, 400.0), 0.0, "the first frame: the start");
        assert_eq!(centred_scroll(916.0, 72.0, 1000.0, 400.0), 600.0, "the last frame: the end");
        assert_eq!(centred_scroll(200.0, 72.0, 300.0, 400.0), 0.0, "no overflow, no scroll");
    }

    #[test]
    fn the_layout_places_frames_by_index() {
        let l = StripLayout { frame: 72.0, gap: 4.0, padding: 12.0 };
        assert_eq!(l.frame_left(0), 12.0);
        assert_eq!(l.frame_left(3), 12.0 + 3.0 * 76.0);
        assert_eq!(l.content_width(3), 24.0 + 3.0 * 72.0 + 2.0 * 4.0);
        assert_eq!(l.content_width(0), 24.0);
        // 30 frames (2300 px) in 1000 px: frame 15's centre minus half the viewport.
        assert_eq!(l.centre(15, 30, 1000.0), 12.0 + 15.0 * 76.0 + 36.0 - 500.0);
        assert_eq!(l.centre(0, 30, 1000.0), 0.0);
        assert_eq!(l.centre(29, 30, 1000.0), l.content_width(30) - 1000.0);
    }

    #[test]
    fn a_cover_token_names_the_version_and_its_revision() {
        assert_eq!(cover_look(Some("12:3")), Some(CoverLook { version: 12, rev: 3 }));
        assert_eq!(cover_look(None), None);
        assert_eq!(cover_look(Some("12")), None);
        assert_eq!(cover_look(Some("x:3")), None);
        assert_ne!(cover_look(Some("12:3")), cover_look(Some("12:4")), "a new revision is a new look");
    }
}
