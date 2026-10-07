//! The Slideshow dialog's choices and play order. Port of the pure parts of
//! `src/modules/plugins/SlideshowDialog.tsx`: the orientation × resolution presets
//! (`videoDims`), the drag-to-reorder step, and the progress label. See docs/slideshow.md.

/// Output orientation (for mobile use, pick Portrait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Landscape,
    Portrait,
    Square,
}

impl Orientation {
    pub const ALL: [Orientation; 3] = [Orientation::Landscape, Orientation::Portrait, Orientation::Square];

    pub fn label(self) -> &'static str {
        match self {
            Orientation::Landscape => "Landscape (16:9)",
            Orientation::Portrait => "Portrait (9:16)",
            Orientation::Square => "Square (1:1)",
        }
    }
}

/// Output resolution; its base is the short side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Hd,
    Uhd,
}

impl Resolution {
    pub const ALL: [Resolution; 2] = [Resolution::Hd, Resolution::Uhd];

    pub fn label(self) -> &'static str {
        match self {
            Resolution::Hd => "1080p (HD)",
            Resolution::Uhd => "4K",
        }
    }

    fn base(self) -> u32 {
        match self {
            Resolution::Hd => 1080,
            Resolution::Uhd => 2160,
        }
    }
}

/// Output `(width, height)`: the long side is 16:9 of the base (an integer, since both bases
/// divide by 9); portrait swaps; square is base × base.
pub fn video_dims(orientation: Orientation, resolution: Resolution) -> (u32, u32) {
    let base = resolution.base();
    let long = base * 16 / 9;
    match orientation {
        Orientation::Square => (base, base),
        Orientation::Portrait => (base, long),
        Orientation::Landscape => (long, base),
    }
}

/// The frame rates offered.
pub const FPS_CHOICES: [u32; 3] = [24, 30, 60];
/// Per-photo duration slider: 1–15 s in 0.5 s steps.
pub const DURATION_RANGE: (f64, f64, f64) = (1.0, 15.0, 0.5);
/// Crossfade slider: 0.2–3 s in 0.1 s steps.
pub const TRANSITION_RANGE: (f64, f64, f64) = (0.2, 3.0, 0.1);
/// The default output folder.
pub const DEFAULT_DEST: &str = "~/Videos";

/// Move the item at `from` to `to` (the drop target's index), as the dialog's `reorder`.
/// Out-of-range indices change nothing.
pub fn reorder<T>(items: &mut Vec<T>, from: usize, to: usize) {
    if from == to || from >= items.len() || to >= items.len() {
        return;
    }
    let moved = items.remove(from);
    items.insert(to, moved);
}

/// Encode progress as a whole percentage (0–100); 0 before any frame count.
pub fn percent(done: u32, total: u32) -> u32 {
    if total == 0 {
        return 0;
    }
    ((done as f64 / total as f64) * 100.0).round().min(100.0) as u32
}

/// The status line while rendering: indeterminate without a live progress listener,
/// "Preparing frames…" before the first progress, then the percentage.
pub fn progress_label(listening: bool, progress: Option<(u32, u32)>) -> String {
    match (listening, progress) {
        (false, _) => "Rendering…".into(),
        (true, None) => "Preparing frames…".into(),
        (true, Some((done, total))) => format!("Encoding… {}%", percent(done, total)),
    }
}

/// Snap a slider value to its step within `(min, max, step)`.
pub fn snap(value: f64, (min, max, step): (f64, f64, f64)) -> f64 {
    let v = ((value - min) / step).round() * step + min;
    (v.clamp(min, max) * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_match_the_dialog() {
        assert_eq!(video_dims(Orientation::Landscape, Resolution::Hd), (1920, 1080));
        assert_eq!(video_dims(Orientation::Landscape, Resolution::Uhd), (3840, 2160));
        assert_eq!(video_dims(Orientation::Portrait, Resolution::Hd), (1080, 1920));
        assert_eq!(video_dims(Orientation::Square, Resolution::Uhd), (2160, 2160));
    }

    #[test]
    fn reorder_moves_one_item_to_the_drop_index() {
        let mut v = vec![1, 2, 3, 4];
        reorder(&mut v, 0, 2);
        assert_eq!(v, [2, 3, 1, 4]);
        reorder(&mut v, 3, 0);
        assert_eq!(v, [4, 2, 3, 1]);
        reorder(&mut v, 1, 1);
        reorder(&mut v, 9, 0);
        assert_eq!(v, [4, 2, 3, 1]);
    }

    #[test]
    fn progress_reads_like_the_dialog() {
        assert_eq!(progress_label(false, Some((5, 10))), "Rendering…");
        assert_eq!(progress_label(true, None), "Preparing frames…");
        assert_eq!(progress_label(true, Some((1, 3))), "Encoding… 33%");
        assert_eq!(percent(7, 0), 0);
        assert_eq!(percent(20, 10), 100);
    }

    #[test]
    fn sliders_snap_to_their_steps() {
        assert_eq!(snap(4.26, DURATION_RANGE), 4.5);
        assert_eq!(snap(40.0, DURATION_RANGE), 15.0);
        assert_eq!(snap(0.0, TRANSITION_RANGE), 0.2);
        assert_eq!(snap(1.04, TRANSITION_RANGE), 1.0);
    }
}
