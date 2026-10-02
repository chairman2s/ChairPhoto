//! The shell layout's per-machine preferences: React's `localStorage` keys for the side
//! columns, the thumbnail size, the inspector tab and the collection browser's sections, read
//! and written through [`crate::machine_prefs::MachinePrefs`] with the same keys and the same
//! value semantics, so a value means what it meant in the webview.
//!
//! | Key | Value | Default | React |
//! |---|---|---|---|
//! | `panel.leftW`, `panel.rightW` | px, a decimal number | 210, 316 | `App.tsx` |
//! | `panel.leftHidden`, `panel.rightHidden` | `"1"` hidden, anything else shown | shown | `App.tsx` |
//! | `panel.thumbSize` | px, clamped to 120–320 | 160 | `App.tsx` |
//! | `panel.inspectorTab` | `details` / `tags` / `versions` / `publish` | `details` | `App.tsx` |
//! | `panel.section.<id>` (`tags`, `smartAlbums`, `albums`, `batches`) | `"1"` open, `"0"` closed | open | `CollectionBrowser.tsx` |
//!
//! The Photo inspector's `inspector.section.<id>` keys are the inspector's own
//! (`crate::inspector`), default collapsed.
//!
//! Deviation, deliberate: React read a column width back unclamped (a hand-edited `9999` drew a
//! 9999 px column until the next drag); here a stored width is clamped to the drag limits
//! (140–640 px) and a value that is not a number falls back to the default, where React got
//! `NaN`. The narrow-window overlays are never persisted, as in React.

use super::state::{clamp_column, InspectorTab, Layout, Section, THUMB_DEFAULT, THUMB_MAX, THUMB_MIN};

pub const LEFT_W: &str = "panel.leftW";
pub const RIGHT_W: &str = "panel.rightW";
pub const LEFT_HIDDEN: &str = "panel.leftHidden";
pub const RIGHT_HIDDEN: &str = "panel.rightHidden";
pub const THUMB_SIZE: &str = "panel.thumbSize";
pub const INSPECTOR_TAB: &str = "panel.inspectorTab";

/// `panel.section.<id>`, with React's section ids.
pub fn section_key(section: Section) -> String {
    let id = match section {
        Section::Tags => "tags",
        Section::SmartAlbums => "smartAlbums",
        Section::Albums => "albums",
        Section::Batches => "batches",
    };
    format!("panel.section.{id}")
}

/// What the stored preferences restore at launch.
#[derive(Debug, Clone, PartialEq)]
pub struct Restored {
    pub layout: Layout,
    pub inspector_tab: InspectorTab,
    /// Indexed as [`Section::ALL`].
    pub sections_open: [bool; 4],
}

/// Read the layout from `get` (a preference store's lookup); anything missing or unusable
/// takes React's default.
pub fn restore(get: impl Fn(&str) -> Option<String>) -> Restored {
    let width = |key: &str, default: f32| {
        get(key).and_then(|v| v.trim().parse::<f32>().ok()).filter(|w| w.is_finite()).map(clamp_column).unwrap_or(default)
    };
    let defaults = Layout::default();
    let thumb_size = get(THUMB_SIZE)
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(THUMB_MIN, THUMB_MAX))
        .unwrap_or(THUMB_DEFAULT);
    let layout = Layout {
        left_hidden: get(LEFT_HIDDEN).as_deref() == Some("1"),
        right_hidden: get(RIGHT_HIDDEN).as_deref() == Some("1"),
        left_w: width(LEFT_W, defaults.left_w),
        right_w: width(RIGHT_W, defaults.right_w),
        thumb_size,
        ..defaults
    };
    let inspector_tab = get(INSPECTOR_TAB)
        .and_then(|v| InspectorTab::ALL.into_iter().find(|t| t.label() == v))
        .unwrap_or(InspectorTab::Details);
    let sections_open = Section::ALL.map(|s| get(&section_key(s)).is_none_or(|v| v == "1"));
    Restored { layout, inspector_tab, sections_open }
}

/// The layout's own keys and values, as React's effect wrote them on every change.
pub fn layout_entries(layout: &Layout, inspector_tab: InspectorTab) -> [(&'static str, String); 6] {
    let flag = |hidden: bool| if hidden { "1" } else { "0" }.to_string();
    [
        (LEFT_W, layout.left_w.to_string()),
        (RIGHT_W, layout.right_w.to_string()),
        (LEFT_HIDDEN, flag(layout.left_hidden)),
        (RIGHT_HIDDEN, flag(layout.right_hidden)),
        (THUMB_SIZE, layout.thumb_size.to_string()),
        (INSPECTOR_TAB, inspector_tab.label().to_string()),
    ]
}

/// A section's stored value.
pub fn section_value(open: bool) -> &'static str {
    if open {
        "1"
    } else {
        "0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn store(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn nothing_stored_restores_reacts_defaults() {
        let r = restore(store(&[]));
        assert_eq!(r.layout, Layout::default());
        assert_eq!(r.inspector_tab, InspectorTab::Details);
        assert_eq!(r.sections_open, [true; 4], "the browser's sections default to open");
    }

    #[test]
    fn stored_values_restore_with_reacts_meaning() {
        let r = restore(store(&[
            (LEFT_W, "233.5"),
            (RIGHT_W, "400"),
            (LEFT_HIDDEN, "1"),
            (RIGHT_HIDDEN, "0"),
            (THUMB_SIZE, "200"),
            (INSPECTOR_TAB, "versions"),
            ("panel.section.smartAlbums", "0"),
            ("panel.section.batches", "1"),
        ]));
        assert_eq!((r.layout.left_w, r.layout.right_w), (233.5, 400.));
        assert!(r.layout.left_hidden && !r.layout.right_hidden);
        assert_eq!(r.layout.thumb_size, 200.);
        assert_eq!(r.inspector_tab, InspectorTab::Versions);
        assert_eq!(r.sections_open, [true, false, true, true]);
        assert!(!r.layout.overlay_left && !r.layout.overlay_right, "overlays are never restored");
    }

    /// React clamped the thumbnail size and whitelisted the tab; a width out of the drag
    /// limits is clamped too, and garbage falls back to the default.
    #[test]
    fn unusable_values_fall_back_or_clamp() {
        let r = restore(store(&[
            (LEFT_W, "9999"),
            (RIGHT_W, "wide"),
            (LEFT_HIDDEN, "true"),
            (THUMB_SIZE, "12"),
            (INSPECTOR_TAB, "faces"),
            ("panel.section.tags", "yes"),
        ]));
        assert_eq!(r.layout.left_w, 640.);
        assert_eq!(r.layout.right_w, 316.);
        assert!(!r.layout.left_hidden, "only \"1\" hides");
        assert_eq!(r.layout.thumb_size, 120.);
        assert_eq!(r.inspector_tab, InspectorTab::Details);
        assert!(!r.sections_open[0], "a stored section value other than \"1\" is closed, as React read it");
        assert_eq!(restore(store(&[(THUMB_SIZE, "NaN")])).layout.thumb_size, 160.);
    }

    /// What is written reads back as the same layout.
    #[test]
    fn written_entries_round_trip() {
        let layout = Layout { left_hidden: true, left_w: 250., right_w: 333.25, thumb_size: 248., ..Layout::default() };
        let entries = layout_entries(&layout, InspectorTab::Publish);
        assert_eq!(entries[0], (LEFT_W, "250".to_string()), "a whole width is written as JavaScript wrote it");
        assert_eq!(entries[2], (LEFT_HIDDEN, "1".to_string()));
        let map: Vec<(&str, &str)> = entries.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = restore(store(&map));
        assert_eq!(r.layout, layout);
        assert_eq!(r.inspector_tab, InspectorTab::Publish);
        assert_eq!(section_key(Section::SmartAlbums), "panel.section.smartAlbums");
        assert_eq!((section_value(true), section_value(false)), ("1", "0"));
    }
}
