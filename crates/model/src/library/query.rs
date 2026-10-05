//! The library view's data: which photos match the current query, and the storage badges
//! that hang off them. Port of `src/modules/libraryQuery.ts` (`useLibraryQuery`).
//!
//! Two failures this exists to make hard to write:
//!
//! 1. **Badges for the whole result.** Storage status cannot ride the photo row (deriving it
//!    needs volume reachability, stat-ed off the catalog lock), so it is fetched only for
//!    the rows the grid says are on screen, plus the ids a caller pins (the active photo).
//! 2. **Late responses winning.** Every refresh takes a generation token, and a response
//!    whose token is stale — a newer refresh started, or [`LibraryQuery::clear`] ran — is
//!    dropped rather than written.
//!
//! ### Shape: a state machine, not a hook
//!
//! The TS hook awaited `listPhotos` / `photoStatuses` itself. This port does no I/O: each
//! step that would have sent a request *returns* it, tagged with its generation, and the
//! host (the GPUI view, on its executor) performs it and hands the answer back.
//!
//! | TS | here |
//! |---|---|
//! | `await refresh()` | [`LibraryQuery::refresh`] → [`RefreshRequest`]; run `list_photos(req.query)`; [`LibraryQuery::apply_page`] |
//! | `photoStatuses(ids)` fired internally | a returned [`StatusRequest`]; run `photo_statuses(req.ids)`; [`LibraryQuery::apply_statuses`] |
//! | `setVisibleRange` / `requestStatuses` | same names, returning `Option<StatusRequest>` |
//! | `clear` | [`LibraryQuery::clear`] |
//!
//! That keeps the TS's forced-interleaving tests meaningful: a test holds any request and
//! answers it in whatever order it likes.
//!
//! Semantic choices against the TypeScript:
//! - **A failed query** was a rejected promise the caller reported, with the rows left
//!   alone. [`LibraryQuery::apply_page`] takes the `Result` and returns the error unchanged,
//!   touching nothing — whichever generation it belonged to, as in the TS.
//! - **A failed status fetch** was swallowed; if its generation is still current the ids
//!   become askable again. [`LibraryQuery::apply_statuses`] does the same with an `Err`.
//! - **`slice(start, end)`** clamps to the row count and yields nothing when `start >= end`;
//!   the port does the same (`usize`, so no negative indices).
//! - **`statuses`** was a `Map` read by id; it is a `BTreeMap`, so iteration is by id rather
//!   than by arrival order. Nothing read the order.
//! - `total` is `usize` (the backend's `PhotoPage::total`).

use chairphoto_core::catalog::{CoverPin, Photo, PhotoPage, PhotoQuery, StorageStatus};
use std::collections::{BTreeMap, HashSet};

/// A `list_photos` call the host must make, and the generation its answer belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshRequest {
    pub generation: u64,
    pub query: PhotoQuery,
}

/// A `photo_statuses` call the host must make, for `ids`, under `generation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRequest {
    pub generation: u64,
    pub ids: Vec<i64>,
}

/// The library view's state: the rows, the total, and the badges fetched so far.
#[derive(Debug, Default)]
pub struct LibraryQuery {
    photos: Vec<Photo>,
    total: usize,
    statuses: BTreeMap<i64, StorageStatus>,
    /// Bumped by every refresh and by `clear`; a response carrying another value is stale.
    generation: u64,
    /// Bumped only when a refresh's rows actually land (`apply_page`, landed) or are disowned
    /// (`clear`) — unlike `generation`, which bumps as soon as a refresh is *requested*, before
    /// its answer arrives. A caller that must redo work keyed off the rows themselves (#191 M2)
    /// wants this counter, not `generation`: keying off `generation` caches a window built from
    /// the still-old `photos()` under the number the *next* refresh's answer will also carry,
    /// so that stale window survives the real landing.
    rows_landed: u64,
    /// Ids already requested under the current generation, so scrolling back over rows does
    /// not re-ask for them. Cleared with each applied refresh: a new query means new answers.
    requested: HashSet<i64>,
    /// The visible range the grid last reported, half-open `[start, end)` over `photos`.
    range: (usize, usize),
    /// The ids most recently asked for by id rather than by window — in practice the active
    /// photo. Re-fetched with every refresh (which clears the badges), so the inspector's
    /// storage line does not go blank when the active photo is outside the grid's window.
    /// Replaced, not accumulated.
    pinned: Vec<i64>,
}

impl LibraryQuery {
    pub fn new() -> Self {
        Self::default()
    }

    /// The rows the current query returned.
    pub fn photos(&self) -> &[Photo] {
        &self.photos
    }

    /// How many photos match the query (equals `photos().len()` until the grid windows).
    pub fn total(&self) -> usize {
        self.total
    }

    /// Storage status for the rows that have been asked for, by photo id.
    pub fn statuses(&self) -> &BTreeMap<i64, StorageStatus> {
        &self.statuses
    }

    /// Mark the not-yet-requested `ids` as requested and return the request for them.
    fn fetch_statuses(&mut self, generation: u64, ids: impl IntoIterator<Item = i64>) -> Option<StatusRequest> {
        let missing: Vec<i64> = ids.into_iter().filter(|id| !self.requested.contains(id)).collect();
        if missing.is_empty() {
            return None;
        }
        self.requested.extend(missing.iter().copied());
        Some(StatusRequest { generation, ids: missing })
    }

    fn window_ids(&self, start: usize, end: usize) -> Vec<i64> {
        let end = end.min(self.photos.len());
        let start = start.min(end);
        self.photos[start..end].iter().map(|p| p.id).collect()
    }

    /// Ask for specific photos' storage status — rows the grid never showed, such as the
    /// photo a deep link opened straight into the loupe. Ids already fetched are skipped.
    /// These ids are also re-asked after every refresh, until replaced.
    pub fn request_statuses(&mut self, ids: &[i64]) -> Option<StatusRequest> {
        self.pinned = ids.to_vec();
        self.fetch_statuses(self.generation, ids.iter().copied())
    }

    /// Report which rows are on screen, as a half-open `[start, end)` over `photos()`.
    pub fn set_visible_range(&mut self, start: usize, end: usize) -> Option<StatusRequest> {
        self.range = (start, end);
        let ids = self.window_ids(start, end);
        self.fetch_statuses(self.generation, ids)
    }

    /// Start re-running `query`. Every earlier request is stale from here on.
    pub fn refresh(&mut self, query: &PhotoQuery) -> RefreshRequest {
        self.generation += 1;
        RefreshRequest { generation: self.generation, query: query.clone() }
    }

    /// The newest refresh's generation: a page for any other is stale.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Bumped each time `photos()` actually changed under a landed refresh or a `clear` —
    /// never merely because one was requested (#191 M2; see the field doc).
    pub fn rows_landed(&self) -> u64 {
        self.rows_landed
    }

    /// One row's face (#252) read again on its own — its look token and pin — instead of
    /// re-reading every row. Returns whether the row is listed and its face changed; then it
    /// counts as rows landed, so the views keyed by them ask for the new look.
    pub fn patch_face(&mut self, id: i64, cover_token: Option<String>, cover_pin: CoverPin) -> bool {
        let Some(row) = self.photos.iter_mut().find(|p| p.id == id) else { return false };
        if row.cover_token == cover_token && row.cover_pin == cover_pin {
            return false;
        }
        row.cover_token = cover_token;
        row.cover_pin = cover_pin;
        self.rows_landed += 1;
        true
    }

    /// Hand back a refresh's answer.
    ///
    /// - `Err`: returned unchanged, nothing touched — an empty grid would read as "no photos
    ///   match", not "the query failed". Reporting it is the caller's job.
    /// - `Ok` from a stale generation: dropped (`Ok(None)`).
    /// - `Ok` and current: the rows, total and badges are replaced, and the badges for the
    ///   last reported window plus the pinned ids are asked for again — a refresh that left
    ///   the scroll position alone gets no new window report, but it did clear the badges.
    pub fn apply_page<E>(
        &mut self,
        request: &RefreshRequest,
        result: Result<PhotoPage, E>,
    ) -> Result<Option<StatusRequest>, E> {
        let page = result?;
        if request.generation != self.generation {
            return Ok(None);
        }
        self.rows_landed += 1;
        self.photos = page.photos;
        self.total = page.total;
        self.requested.clear();
        self.statuses.clear();
        let (start, end) = self.range;
        let mut ids = self.window_ids(start, end);
        ids.extend(self.pinned.iter().copied());
        Ok(self.fetch_statuses(request.generation, ids))
    }

    /// Hand back a status fetch's answer. A stale one is dropped. A failed current one makes
    /// its ids askable again, so a later window (or a retry) asks again.
    pub fn apply_statuses<E>(&mut self, request: &StatusRequest, result: Result<Vec<(i64, StorageStatus)>, E>) {
        if request.generation != self.generation {
            return;
        }
        match result {
            Ok(pairs) => self.statuses.extend(pairs),
            Err(_) => {
                for id in &request.ids {
                    self.requested.remove(id);
                }
            }
        }
    }

    /// Drop everything, and disown whatever is in flight. For a catalog switch: the rows
    /// belong to a catalog that is no longer open, and a refresh still running against it
    /// must not land.
    pub fn clear(&mut self) {
        // Bumping the generation is the disowning.
        self.generation += 1;
        self.rows_landed += 1;
        self.photos.clear();
        self.requested.clear();
        self.pinned.clear();
        self.total = 0;
        self.statuses.clear();
    }
}

// Port of `src/modules/__tests__/libraryQuery.test.tsx` (8 cases, same names, grouped by
// `describe` block). The TS resolved hand-held promises in a chosen order; here every request
// is a returned value, so "the old refresh answers last" is just the order of the calls.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::test_support::page;
    use StorageStatus::*;

    fn empty_query() -> PhotoQuery {
        PhotoQuery::default()
    }

    /// Refresh and apply `ids` at once, as `await refresh()` with an immediately-resolving
    /// `listPhotos` did.
    fn refresh_with(lq: &mut LibraryQuery, ids: &[i64]) -> Option<StatusRequest> {
        let req = lq.refresh(&empty_query());
        lq.apply_page::<()>(&req, Ok(page(ids, ids.len()))).unwrap()
    }

    mod a_late_response_from_an_older_refresh {
        use super::*;

        #[test]
        fn cannot_overwrite_the_badges_a_newer_refresh_already_wrote() {
            let mut lq = LibraryQuery::new();
            // The grid is showing its first rows; from here on a refresh re-asks for this window.
            assert_eq!(lq.set_visible_range(0, 2), None);

            let old_badges = refresh_with(&mut lq, &[1, 2]).unwrap();
            assert_eq!(old_badges.ids, vec![1, 2]);
            let new_badges = refresh_with(&mut lq, &[3, 4]).unwrap();
            assert_eq!(new_badges.ids, vec![3, 4]);

            // The newer refresh answers first and paints the grid.
            lq.apply_statuses::<()>(&new_badges, Ok(vec![(3, BackedUp)]));
            assert_eq!(lq.statuses().get(&3), Some(&BackedUp));

            // …and now the previous filter's badge fetch finally comes back.
            lq.apply_statuses::<()>(&old_badges, Ok(vec![(1, Missing), (2, Missing)]));

            assert_eq!(lq.statuses().get(&3), Some(&BackedUp));
            assert!(!lq.statuses().contains_key(&1));
            assert!(!lq.statuses().contains_key(&2));
            assert_eq!(lq.statuses().keys().copied().collect::<Vec<_>>(), vec![3]);
        }

        #[test]
        fn cannot_replace_the_photo_rows_a_newer_refresh_already_wrote() {
            let mut lq = LibraryQuery::new();
            // Both in flight at once — the second overtakes the first.
            let first = lq.refresh(&empty_query());
            let second = lq.refresh(&empty_query());

            lq.apply_page::<()>(&second, Ok(page(&[3, 4], 2))).unwrap();
            assert_eq!(ids(&lq), vec![3, 4]);

            lq.apply_page::<()>(&first, Ok(page(&[1, 2], 999))).unwrap();
            assert_eq!(ids(&lq), vec![3, 4]);
            assert_eq!(lq.total(), 2);
        }

        #[test]
        fn reports_a_failed_query_to_its_caller_without_blanking_the_newer_result() {
            let mut lq = LibraryQuery::new();
            let first = lq.refresh(&empty_query());
            refresh_with(&mut lq, &[3, 4]);
            assert_eq!(ids(&lq), vec![3, 4]);

            // The failure reaches whoever asked — but the rows are left alone.
            let err = lq.apply_page(&first, Err("catalog closed")).unwrap_err();
            assert_eq!(err, "catalog closed");
            assert_eq!(ids(&lq), vec![3, 4]);
        }
    }

    mod badges_follow_the_visible_window {
        use super::*;

        #[test]
        fn fetches_storage_status_for_the_reported_rows_not_for_every_matching_photo() {
            let all: Vec<i64> = (1..=1000).collect();
            let mut lq = LibraryQuery::new();
            // Nothing is on screen yet, so nothing has been asked for.
            assert_eq!(refresh_with(&mut lq, &all), None);

            let call0 = lq.set_visible_range(0, 30).unwrap();
            assert_eq!(call0.ids.len(), 30);
            assert_eq!(call0.ids[0], 1);
            lq.apply_statuses::<()>(&call0, Ok(vec![]));

            // Scrolling asks only for the rows it newly revealed.
            let call1 = lq.set_visible_range(20, 60).unwrap();
            assert_eq!(call1.ids.len(), 30);
            assert_eq!(call1.ids[0], 31);
            lq.apply_statuses::<()>(&call1, Ok(vec![]));

            // Scrolling back asks for nothing: those rows are already in hand.
            assert_eq!(lq.set_visible_range(0, 30), None);
        }

        #[test]
        fn re_fetches_the_last_reported_window_after_a_refresh_cleared_the_badges() {
            let mut lq = LibraryQuery::new();
            assert_eq!(refresh_with(&mut lq, &[1, 2, 3, 4]), None);
            let call0 = lq.set_visible_range(0, 2).unwrap();
            lq.apply_statuses::<()>(&call0, Ok(vec![(1, LocalOnly)]));

            // A refresh (a rating change, a scan commit) leaves the scroll position alone, so
            // the grid never reports again — the refresh itself has to re-ask.
            let call1 = refresh_with(&mut lq, &[1, 2, 3, 4]).unwrap();
            assert_eq!(call1.ids, vec![1, 2]);
        }

        #[test]
        fn asks_for_a_photo_the_grid_never_showed_on_request() {
            let mut lq = LibraryQuery::new();
            refresh_with(&mut lq, &[1, 2, 3]);
            let call0 = lq.request_statuses(&[3]).unwrap();
            assert_eq!(call0.ids, vec![3]);
            lq.apply_statuses::<()>(&call0, Ok(vec![(3, Archived)]));
            assert_eq!(lq.statuses().get(&3), Some(&Archived));
        }

        #[test]
        fn keeps_the_requested_photo_s_badge_across_a_refresh_that_cleared_it() {
            // The active photo is row 99 — far outside the two rows the grid is showing — and
            // the user rates it, which refreshes. Without re-asking, the inspector's storage
            // line would go blank until the selection changed. (The TS mock answered every
            // status call with `[[99, "offline"]]`; so does this test.)
            let answer = || Ok::<_, ()>(vec![(99, Offline)]);
            let all: Vec<i64> = (1..=100).collect();
            let mut lq = LibraryQuery::new();
            refresh_with(&mut lq, &all);
            let call0 = lq.set_visible_range(0, 2).unwrap();
            lq.apply_statuses(&call0, answer());
            let call1 = lq.request_statuses(&[99]).unwrap();
            lq.apply_statuses(&call1, answer());
            assert_eq!(lq.statuses().get(&99), Some(&Offline));

            let call2 = refresh_with(&mut lq, &all).unwrap();
            assert_eq!(call2.ids, vec![1, 2, 99]);
            lq.apply_statuses(&call2, answer());
            assert_eq!(lq.statuses().get(&99), Some(&Offline));
        }
    }

    mod clear {
        use super::*;

        #[test]
        fn drops_the_rows_and_disowns_a_refresh_still_in_flight() {
            let mut lq = LibraryQuery::new();
            assert_eq!(lq.set_visible_range(0, 2), None);
            let badges = refresh_with(&mut lq, &[1, 2]).unwrap();
            lq.apply_statuses::<()>(&badges, Ok(vec![]));
            assert_eq!(lq.photos().len(), 2);

            let switching = lq.refresh(&empty_query());
            lq.clear();
            assert_eq!(lq.photos().len(), 0);
            assert_eq!(lq.total(), 0);
            assert_eq!(lq.statuses().len(), 0);

            // The catalog that request belonged to is closed; its answer must not repopulate
            // the grid behind the switch.
            lq.apply_page::<()>(&switching, Ok(page(&[7, 8], 2))).unwrap();
            assert_eq!(lq.photos().len(), 0);
        }
    }

    fn ids(lq: &LibraryQuery) -> Vec<i64> {
        lq.photos().iter().map(|p| p.id).collect()
    }
}
