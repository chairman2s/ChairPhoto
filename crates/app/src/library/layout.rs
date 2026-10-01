//! The grid's arithmetic and the tile vocabulary, without GPUI: how many columns fit, how
//! tall a row is, which rows to load, and what each badge and empty state says. Ports of
//! `CatalogGrid.tsx`'s `computeCols`, its tile overlay and `Thumbnail.tsx`'s failure labels.

use chairphoto_core::catalog::{CullingFilter, StorageStatus, StorageTier};
use chairphoto_model::library::session::LibraryScope;
use std::ops::Range;

/// Pixels between tiles, both ways (`.grid-row` gap).
pub const GAP: f32 = 3.;
/// The filename strip under each thumbnail.
pub const NAME_H: f32 = 22.;
/// Rows loaded beyond the visible ones on each side (React's virtualizer `overscan: 3`):
/// their thumbnails and storage badges are in hand before a small scroll reveals them.
pub const OVERSCAN_ROWS: usize = 3;

/// How many columns fit `width` px at `tile_min` px minimum tile width — CSS grid's
/// `repeat(auto-fill, minmax(tileMin, 1fr))`, React's `computeCols`. Nothing measured yet
/// (`width <= 0`) is one column, not zero.
pub fn columns(width: f32, tile_min: f32) -> usize {
    if width <= 0. {
        return 1;
    }
    (((width + GAP) / (tile_min + GAP)).floor() as usize).max(1)
}

/// One tile's width when `cols` share `width`.
pub fn tile_width(width: f32, cols: usize) -> f32 {
    let cols = cols.max(1) as f32;
    ((width - GAP * (cols - 1.)) / cols).max(1.)
}

/// One row's height: a 3:2 thumbnail, the filename strip, and the gap below.
pub fn row_height(width: f32, cols: usize) -> f32 {
    (tile_width(width, cols) * 2. / 3.).round() + NAME_H + GAP
}

/// The photo indices row range `rows` covers, clamped to `len` photos.
pub fn cells(rows: Range<usize>, cols: usize, len: usize) -> Range<usize> {
    let start = (rows.start * cols).min(len);
    let end = (rows.end * cols).min(len);
    start..end
}

/// The rows to load for `visible` out of `row_count`, most urgent first: the visible rows
/// top to bottom, then the overscan below (the usual scroll direction through a
/// newest-last grid is back up, but a fresh grid opens at the bottom, so either is a
/// guess), then the overscan above.
pub fn wanted_rows(visible: Range<usize>, row_count: usize, overscan: usize) -> Vec<usize> {
    let end = visible.end.min(row_count);
    let start = visible.start.min(end);
    let below = end..(end + overscan).min(row_count);
    let above = start.saturating_sub(overscan)..start;
    (start..end).chain(below).chain(above.rev()).collect()
}

/// The whole span [`wanted_rows`] covers, as rows.
pub fn wanted_span(visible: Range<usize>, row_count: usize, overscan: usize) -> Range<usize> {
    let end = visible.end.min(row_count);
    let start = visible.start.min(end);
    start.saturating_sub(overscan)..(end + overscan).min(row_count)
}

/// The grid's empty-state text, worded by what is filtering it (App.tsx).
pub fn empty_message(scope: &LibraryScope) -> &'static str {
    if scope.storage_tier == StorageTier::Nas {
        return "No NAS-only photos yet. Older photos move here when offloaded — set a day count in \
                Preferences → Storage → Local / NAS tiering, or click “Offload older now”.";
    }
    let filtered = scope.storage_tier == StorageTier::Local
        || scope.filter != CullingFilter::All
        || scope.tag_id.is_some()
        || scope.album_id.is_some()
        || scope.batch_id.is_some()
        || scope.smart_album_id.is_some()
        || !scope.facets.is_empty()
        || !scope.labels.is_empty();
    if filtered {
        "No photos match the current filters."
    } else {
        "No photos. Scan a folder to begin."
    }
}

/// Which storage icons a tile shows (`storageIcons`): a local copy (▣), and a NAS copy
/// (☁) that is reachable (`Some(true)`) or offline (`Some(false)`).
pub fn storage_icons(status: Option<StorageStatus>) -> (bool, Option<bool>) {
    match status {
        Some(StorageStatus::LocalOnly) => (true, None),
        Some(StorageStatus::BackedUp) => (true, Some(true)),
        Some(StorageStatus::Archived) => (false, Some(true)),
        Some(StorageStatus::Offline) => (false, Some(false)),
        Some(StorageStatus::Missing) | None => (false, None),
    }
}

/// What a tile whose thumbnail failed says (`Thumbnail.tsx`): icon and label, by storage
/// status.
pub fn failed_label(status: Option<StorageStatus>) -> (&'static str, &'static str) {
    match status {
        Some(StorageStatus::Archived | StorageStatus::Offline) => ("☁", "On NAS"),
        Some(StorageStatus::Missing) => ("⚠", "Missing"),
        _ => ("⚠", "No preview"),
    }
}

/// The index a page key moves to: `delta` photos from `from` (or from the start when
/// nothing is active), clamped to the rows.
pub fn page_target(len: usize, from: Option<usize>, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let from = from.map_or(if delta < 0 { len as isize } else { -1 }, |i| i as isize);
    Some((from + delta).clamp(0, len as isize - 1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chairphoto_model::library::session::default_scope;

    #[test]
    fn columns_follow_reacts_compute_cols() {
        assert_eq!(columns(0., 160.), 1);
        assert_eq!(columns(-5., 160.), 1);
        assert_eq!(columns(100., 160.), 1, "narrower than one tile is still one column");
        // floor((w + 3) / (160 + 3)): two columns need 2 × 160 + 3 = 323 px.
        assert_eq!(columns(322., 160.), 1);
        assert_eq!(columns(323., 160.), 2);
        assert_eq!(columns(1000., 160.), 6);
        assert_eq!(columns(1000., 320.), 3);
    }

    #[test]
    fn rows_are_a_three_by_two_thumbnail_plus_the_name_and_the_gap() {
        // 6 columns of (1000 − 15) / 6 = 164.17 px: thumbs 109 px high.
        assert_eq!(row_height(1000., 6), 109. + NAME_H + GAP);
        assert!((tile_width(1000., 6) - 164.1667).abs() < 0.01);
    }

    #[test]
    fn cells_clamp_to_the_last_partial_row() {
        assert_eq!(cells(0..2, 4, 10), 0..8);
        assert_eq!(cells(2..3, 4, 10), 8..10);
        assert_eq!(cells(5..9, 4, 10), 10..10);
    }

    #[test]
    fn visible_rows_load_first_then_below_then_above() {
        assert_eq!(wanted_rows(10..13, 100, 2), vec![10, 11, 12, 13, 14, 9, 8]);
        assert_eq!(wanted_rows(0..2, 3, 3), vec![0, 1, 2], "clamped at both ends");
        assert_eq!(wanted_rows(0..0, 0, 3), Vec::<usize>::new());
        assert_eq!(wanted_span(10..13, 100, 2), 8..15);
        assert_eq!(wanted_span(1..3, 4, 3), 0..4);
    }

    #[test]
    fn the_empty_state_says_what_is_filtering() {
        let mut scope = default_scope();
        assert_eq!(empty_message(&scope), "No photos. Scan a folder to begin.");
        scope.labels = vec!["Red".into()];
        assert_eq!(empty_message(&scope), "No photos match the current filters.");
        scope = default_scope();
        scope.filter = CullingFilter::Pick;
        assert_eq!(empty_message(&scope), "No photos match the current filters.");
        scope = default_scope();
        scope.storage_tier = StorageTier::Nas;
        assert!(empty_message(&scope).starts_with("No NAS-only photos yet."));
        // The sort is not a filter.
        scope = default_scope();
        scope.sort = chairphoto_core::catalog::PhotoSort::SharpnessAsc;
        assert_eq!(empty_message(&scope), "No photos. Scan a folder to begin.");
    }

    #[test]
    fn storage_icons_and_failure_labels_follow_the_status() {
        assert_eq!(storage_icons(Some(StorageStatus::BackedUp)), (true, Some(true)));
        assert_eq!(storage_icons(Some(StorageStatus::Offline)), (false, Some(false)));
        assert_eq!(storage_icons(None), (false, None));
        assert_eq!(failed_label(Some(StorageStatus::Archived)), ("☁", "On NAS"));
        assert_eq!(failed_label(Some(StorageStatus::Missing)), ("⚠", "Missing"));
        assert_eq!(failed_label(Some(StorageStatus::LocalOnly)), ("⚠", "No preview"));
    }

    #[test]
    fn page_keys_clamp_to_the_rows() {
        assert_eq!(page_target(0, None, 5), None);
        assert_eq!(page_target(10, Some(3), 4), Some(7));
        assert_eq!(page_target(10, Some(8), 4), Some(9));
        assert_eq!(page_target(10, Some(2), -4), Some(0));
        assert_eq!(page_target(10, None, 4), Some(3), "from before the first row");
        assert_eq!(page_target(10, None, -4), Some(6), "from past the last row");
    }
}
