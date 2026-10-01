//! The Darkroom filmstrip's pure parts — a port of `src/components/darkroom/filmstrip.ts`:
//! which photos to render around the current one, and where a step lands. The strip follows
//! the Library's current order and filter.

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
}
