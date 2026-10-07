//! The Library surface as one session: what the grid is asking for, what is selected, and
//! what a catalog switch has to forget. Port of `src/modules/librarySession.ts`
//! (`useLibrarySession`).
//!
//! [`LibraryQuery`] owns the *result* of a query. This owns everything that decides *which*
//! query to run and *which* rows the user is acting on:
//!
//!  - **Scope.** The four sidebar scopes (tag / album / import batch / smart album) are
//!    mutually exclusive; the culling chip, facets, storage tier, camera, lens, colour
//!    labels and sort AND on top of whichever is active. One value, one set of verbs, one
//!    derived [`PhotoQuery`].
//!  - **Selection.** Plain / Ctrl / Shift semantics, the anchor a Shift range spans from,
//!    keyboard stepping, select-all, and the off-grid stack child the loupe can show even
//!    though the grid never listed it.
//!  - **Reset.** A catalog switch invalidates every id above at once; [`LibrarySession::reset`]
//!    is that transition.
//!
//! What deliberately stays with the view: the tag tree, loupe/develop flags, progress,
//! dialogs and every panel's reload key.
//!
//! ### Identity becomes revisions
//!
//! The TS keyed its refetch on object identity: `query` was memoized, every scope verb
//! returned the *same* scope object when nothing changed (clicking the chip you are already
//! on must not refetch the library), and the resets allocated unconditionally (the same
//! filters over a different catalog are a different question). Rust values have no
//! identity, so the port counts instead:
//!
//! - [`LibrarySession::query_revision`] advances exactly when the TS `query` (and so
//!   `refresh`) would have changed identity: when the derived query's *value* changes, and
//!   unconditionally on [`LibrarySession::reset`] and [`LibrarySession::clear_scope`] (the TS
//!   built fresh `facets`/`labels` arrays there, so even a no-op clear refetched — which a
//!   deep link into a newly imported photo relies on). The view re-runs the query when it
//!   advances.
//! - [`LibrarySession::scope_revision`] does the same for the scope object.
//!
//! One deliberate difference: the TS compared the `batch` row by identity, so re-picking an
//! equal-but-new batch row changed the *scope* (not the query). Here rows compare by value,
//! so it changes neither. Nothing keyed on scope identity alone.
//!
//! ### I/O and effects
//!
//! The session does no I/O; see [`super::query`] for the request/answer protocol
//! ([`LibrarySession::refresh`], [`LibrarySession::apply_page`], …). The TS's one effect —
//! ask for the active photo's storage badge whenever the active id changes, because a photo
//! reached by deep link or stack view was never a grid row — is
//! [`LibrarySession::take_active_status_request`], which the view calls after handling
//! input, as React ran the effect after a commit.
//!
//! ### Stale rows for auto-advance
//!
//! The TS selection handlers closed over the rows of the render they were created in, and
//! the culling shortcut relied on it: rate, let the refresh land, *then* step — over the rows
//! it started from, not the refreshed ones the rated photo may have been filtered out of.
//! Here [`LibrarySession::step_active`] steps over the current rows, and
//! [`LibrarySession::step_active_over`] over a [`StepSnapshot`] the caller took
//! ([`LibrarySession::step_snapshot`]) before the refresh. The snapshot holds the active
//! photo as well as the rows, because the TS closure captured both: two culling actions
//! started on the same photo must both step from *that* photo, not from wherever the first
//! one's step left the selection.
//!
//! ### Other semantic choices
//!
//! - Ids are `i64`; `null` is `None`. `delta` is `isize`. An active photo that is in no row
//!   (an off-grid stack child) steps from index −1, as the TS's `findIndex` did, so a forward
//!   step lands on the first row.
//! - `selection.photos` keeps row order (it filtered `photos` by the id list), not selection
//!   order.
//!
//! ### Not yet windowed
//!
//! Issue #10 built `PhotoQuery.window` end to end, but the session still asks for every
//! matching row, because Shift ranges, select-all and the active row all index into the row
//! list; windowing needs the ordered ids of the matching set (not exposed by the backend) or
//! a selection expressed as a range over the query. See the TS module's header for the
//! measurements.

use super::query::{LibraryQuery, RefreshRequest, StatusRequest};
use chairphoto_core::catalog::{
    CullingFilter, ImportBatch, Photo, PhotoPage, PhotoQuery, PhotoSort, StorageStatus, StorageTier,
};
use std::collections::{BTreeMap, HashSet};

/// Everything that decides which photos the Library is showing.
///
/// Apart from `batch`, field for field [`PhotoQuery`], so the derived query is a rename
/// (`filter` → `culling_filter`). `batch` rides along because the export panel and the
/// filter bar need the row, not just its id.
#[derive(Debug, Clone)]
pub struct LibraryScope {
    /// Restrict to a tag and its descendants. Excludes the other three scopes.
    pub tag_id: Option<i64>,
    /// Restrict to an album (which also imposes the album's own member order).
    pub album_id: Option<i64>,
    /// Restrict to one import batch.
    pub batch_id: Option<i64>,
    /// The batch row behind `batch_id`, for panels that need its label, not just its id.
    pub batch: Option<ImportBatch>,
    /// Evaluate a saved smart-album rule live.
    pub smart_album_id: Option<i64>,
    /// The culling chip: all / unrated / pick / reject / edited.
    pub filter: CullingFilter,
    pub storage_tier: StorageTier,
    /// Derived boolean facets, ANDed in.
    pub facets: Vec<String>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    /// Colour labels, OR-combined (`""` = the No-label dot). Empty = no label filter.
    pub labels: Vec<String>,
    pub sort: PhotoSort,
}

/// The whole library, oldest first — what a fresh catalog opens on.
pub fn default_scope() -> LibraryScope {
    LibraryScope {
        tag_id: None,
        album_id: None,
        batch_id: None,
        batch: None,
        smart_album_id: None,
        filter: CullingFilter::All,
        storage_tier: StorageTier::All,
        facets: Vec::new(),
        camera: None,
        lens: None,
        labels: Vec::new(),
        sort: PhotoSort::Date,
    }
}

impl Default for LibraryScope {
    fn default() -> Self {
        default_scope()
    }
}

/// `ImportBatch` has no `PartialEq` in core; compare its fields.
fn batch_eq(a: &ImportBatch, b: &ImportBatch) -> bool {
    a.id == b.id
        && a.uuid == b.uuid
        && a.source_label == b.source_label
        && a.note == b.note
        && a.created_at == b.created_at
        && a.photo_count == b.photo_count
}

impl PartialEq for LibraryScope {
    fn eq(&self, other: &Self) -> bool {
        let batch_same = match (&self.batch, &other.batch) {
            (None, None) => true,
            (Some(a), Some(b)) => batch_eq(a, b),
            _ => false,
        };
        batch_same
            && self.tag_id == other.tag_id
            && self.album_id == other.album_id
            && self.batch_id == other.batch_id
            && self.smart_album_id == other.smart_album_id
            && self.filter == other.filter
            && self.storage_tier == other.storage_tier
            && self.facets == other.facets
            && self.camera == other.camera
            && self.lens == other.lens
            && self.labels == other.labels
            && self.sort == other.sort
    }
}

impl LibraryScope {
    /// The scope as the one typed query the backend takes. `batch` is the session's, not the
    /// query's — the backend filters on the id. No window (see the module docs).
    pub fn query(&self) -> PhotoQuery {
        PhotoQuery {
            tag_id: self.tag_id,
            album_id: self.album_id,
            batch_id: self.batch_id,
            smart_album_id: self.smart_album_id,
            facets: self.facets.clone(),
            culling_filter: self.filter,
            storage_tier: self.storage_tier,
            camera: self.camera.clone(),
            lens: self.lens.clone(),
            labels: self.labels.clone(),
            sort: self.sort,
            window: None,
        }
    }
}

/// Modifier keys as the grid reports them from a click.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectMods {
    pub ctrl: bool,
    pub shift: bool,
}

impl SelectMods {
    pub const CTRL: Self = Self { ctrl: true, shift: false };
    pub const SHIFT: Self = Self { ctrl: false, shift: true };
}

/// Who the user is acting on, derived from the session on demand.
#[derive(Debug)]
pub struct LibrarySelection<'a> {
    /// The active/primary photo — the one the inspector, loupe and Develop view show.
    pub active_id: Option<i64>,
    /// Every selected id, in selection order.
    pub ids: &'a [i64],
    /// The active photo's row: normally the selected grid tile, or the off-grid stack child
    /// being viewed. `None` when the active id names no row we hold (mid-refresh, say).
    pub active: Option<&'a Photo>,
    /// The selected rows that are actually in the current result, in row order.
    pub photos: Vec<&'a Photo>,
    /// What a bulk action applies to: the whole selection, or the active photo when nothing
    /// is selected. Empty when neither exists, so callers can act unconditionally.
    pub targets: Vec<i64>,
    /// A photo outside the grid's result — a stacked child, which the grid hides under its
    /// master — that is nevertheless being viewed.
    pub extra_photo: Option<&'a Photo>,
    /// The master to return to from a stack-child view ("Back to original").
    pub stack_origin: Option<i64>,
}

/// `[lo..=hi]` of `rows` between the positions of `from` and `to`, or `None` when either is
/// not a row.
fn range_between<T>(rows: &[T], key: impl Fn(&T) -> i64, from: i64, to: i64) -> Option<Vec<i64>> {
    let a = rows.iter().position(|r| key(r) == from)?;
    let b = rows.iter().position(|r| key(r) == to)?;
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    Some(rows[lo..=hi].iter().map(key).collect())
}

/// The Library surface's state, and the verbs that move it.
#[derive(Debug)]
pub struct LibrarySession {
    scope: LibraryScope,
    scope_revision: u64,
    query_revision: u64,
    library: LibraryQuery,
    active_id: Option<i64>,
    ids: Vec<i64>,
    /// Fixed anchor for Shift ranges (Shift-click and Shift+Arrow). Set on any plain/toggle
    /// selection; the range spans anchor↔active while Shift extends it.
    anchor: Option<i64>,
    extra_photo: Option<Photo>,
    stack_origin: Option<i64>,
    /// The active id the storage-badge effect last ran for.
    status_effect_for: Option<i64>,
}

impl Default for LibrarySession {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`LibrarySession::step_active_over`] steps over: the rows and the active photo at
/// the moment a culling action started (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSnapshot {
    pub rows: Vec<i64>,
    pub active_id: Option<i64>,
}

impl LibrarySession {
    pub fn new() -> Self {
        Self {
            scope: default_scope(),
            scope_revision: 0,
            query_revision: 0,
            library: LibraryQuery::new(),
            active_id: None,
            ids: Vec::new(),
            anchor: None,
            extra_photo: None,
            stack_origin: None,
            status_effect_for: None,
        }
    }

    // --- the rows ----------------------------------------------------------------------

    /// The rows the current query returned.
    pub fn photos(&self) -> &[Photo] {
        self.library.photos()
    }

    /// The current rows' ids, in order — the snapshot [`Self::step_active_over`] takes.
    pub fn photo_ids(&self) -> Vec<i64> {
        self.photos().iter().map(|p| p.id).collect()
    }

    /// How many photos match the query.
    pub fn total(&self) -> usize {
        self.library.total()
    }

    /// Storage status for the rows that have been asked for, by photo id.
    pub fn statuses(&self) -> &BTreeMap<i64, StorageStatus> {
        self.library.statuses()
    }

    /// The current scope as the backend query.
    pub fn query(&self) -> PhotoQuery {
        self.scope.query()
    }

    /// Advances whenever the view must re-run the query (see the module docs).
    pub fn query_revision(&self) -> u64 {
        self.query_revision
    }

    /// Advances each time a row read actually *lands* ([`Self::apply_page`]) or the rows are
    /// disowned ([`LibraryQuery::clear`] via a catalog switch), whether or not the answer
    /// changes `photos()` — unlike [`query_revision`](Self::query_revision), which only moves
    /// when the query itself changes, and unlike [`LibraryQuery::generation`], which bumps as
    /// soon as a refresh is *requested*, before its rows arrive (#191 M2: keying a cache off
    /// that requested generation caches a window built from the still-old rows under the
    /// number the refresh's own answer will carry, so the stale window survives the landing).
    /// A caller that must redo work whenever the rows were *re-read* (a cover's look or a
    /// version count may be new even though the filter didn't move — e.g. after leaving
    /// Develop) wants this, not `query_revision`.
    pub fn rows_generation(&self) -> u64 {
        self.library.rows_landed()
    }

    /// One row's face read again (see [`LibraryQuery::patch_face`]).
    pub fn patch_face(&mut self, id: i64, cover_token: Option<String>, cover_pin: chairphoto_core::catalog::CoverPin) -> bool {
        self.library.patch_face(id, cover_token, cover_pin)
    }

    /// The newest refresh's generation ([`LibraryQuery::generation`]): bumped when a refresh
    /// is asked for, before its rows land.
    pub fn generation(&self) -> u64 {
        self.library.generation()
    }

    /// Re-run the query: perform the returned request, then [`Self::apply_page`].
    pub fn refresh(&mut self) -> RefreshRequest {
        let query = self.query();
        self.library.refresh(&query)
    }

    /// Hand back a refresh's answer (see [`LibraryQuery::apply_page`]).
    pub fn apply_page<E>(
        &mut self,
        request: &RefreshRequest,
        result: Result<PhotoPage, E>,
    ) -> Result<Option<StatusRequest>, E> {
        let landed = request.generation == self.library.generation();
        let statuses = self.library.apply_page(request, result)?;
        if landed {
            self.trim_selection_to_rows();
        }
        Ok(statuses)
    }

    /// Unselect `gone` (photos just trashed or removed) at once, before the refresh that drops
    /// their rows lands: a key pressed in between must not reach them.
    pub fn unselect(&mut self, gone: &[i64]) {
        let gone: HashSet<i64> = gone.iter().copied().collect();
        self.ids.retain(|id| !gone.contains(id));
        if self.active_id.is_some_and(|a| gone.contains(&a)) {
            self.active_id = None;
        }
        if self.anchor.is_some_and(|a| gone.contains(&a)) {
            self.anchor = None;
        }
        if self.extra_photo.as_ref().is_some_and(|p| gone.contains(&p.id)) {
            self.extra_photo = None;
            self.stack_origin = None;
        }
    }

    /// The selection keeps only photos the rows still list. A photo a filter now hides, or
    /// that left the library (trashed, removed), is no longer selected — so no bulk action
    /// (a rating key, a flag, a label, a tag paste, Move to trash) reaches a photo the user
    /// cannot see. The off-grid stack child the loupe shows stays: it is never a row.
    ///
    /// Not in the TS, which kept hidden ids selected; the grid context menu (#158) made that
    /// reachable as "trash what you cannot see".
    fn trim_selection_to_rows(&mut self) {
        let rows: HashSet<i64> = self.photos().iter().map(|p| p.id).collect();
        let off_grid = self.extra_photo.as_ref().map(|p| p.id);
        let keep = |id: &i64| rows.contains(id) || Some(*id) == off_grid;
        self.ids.retain(keep);
        if self.active_id.is_some_and(|a| !keep(&a)) {
            self.active_id = None;
        }
        if self.anchor.is_some_and(|a| !keep(&a)) {
            self.anchor = None;
        }
    }

    /// Hand back a storage-status answer (see [`LibraryQuery::apply_statuses`]).
    pub fn apply_statuses<E>(&mut self, request: &StatusRequest, result: Result<Vec<(i64, StorageStatus)>, E>) {
        self.library.apply_statuses(request, result)
    }

    /// Report which rows are on screen, as a half-open `[start, end)` over `photos()`.
    pub fn set_visible_range(&mut self, start: usize, end: usize) -> Option<StatusRequest> {
        self.library.set_visible_range(start, end)
    }

    // --- the scope ---------------------------------------------------------------------

    pub fn scope(&self) -> &LibraryScope {
        &self.scope
    }

    /// Advances whenever the scope changes (see the module docs).
    pub fn scope_revision(&self) -> u64 {
        self.scope_revision
    }

    /// Change the scope. Revisions advance when the value changed, or always with `fresh`
    /// (where the TS allocated a new scope unconditionally).
    fn update_scope(&mut self, fresh: bool, f: impl FnOnce(&mut LibraryScope)) {
        let before_scope = self.scope.clone();
        let before_query = self.scope.query();
        f(&mut self.scope);
        if fresh || self.scope != before_scope {
            self.scope_revision += 1;
        }
        if fresh || self.scope.query() != before_query {
            self.query_revision += 1;
        }
    }

    // The four scopes are one primary filter each (the culling chips and facets still AND
    // on top — see docs/smart-albums.md), so picking one clears the other three. Clearing one
    // leaves the others alone: there is nothing to be exclusive with.

    /// Pick a tag scope (or clear it). Picking clears the other three scopes.
    pub fn select_tag(&mut self, tag_id: Option<i64>) {
        self.update_scope(false, |s| {
            s.tag_id = tag_id;
            if tag_id.is_some() {
                s.album_id = None;
                s.batch_id = None;
                s.batch = None;
                s.smart_album_id = None;
            }
        });
    }

    /// Pick an album scope (or clear it). Picking clears the other three scopes.
    pub fn select_album(&mut self, album_id: Option<i64>) {
        self.update_scope(false, |s| {
            s.album_id = album_id;
            if album_id.is_some() {
                s.tag_id = None;
                s.batch_id = None;
                s.batch = None;
                s.smart_album_id = None;
            }
        });
    }

    /// Pick an import-batch scope (or clear it). Picking clears the other three scopes.
    pub fn select_batch(&mut self, batch: Option<ImportBatch>) {
        self.update_scope(false, |s| {
            s.batch_id = batch.as_ref().map(|b| b.id);
            let picked = batch.is_some();
            s.batch = batch;
            if picked {
                s.tag_id = None;
                s.album_id = None;
                s.smart_album_id = None;
            }
        });
    }

    /// Pick a smart-album scope (or clear it). Picking clears the other three scopes.
    pub fn select_smart_album(&mut self, smart_album_id: Option<i64>) {
        self.update_scope(false, |s| {
            s.smart_album_id = smart_album_id;
            if smart_album_id.is_some() {
                s.tag_id = None;
                s.album_id = None;
                s.batch_id = None;
                s.batch = None;
            }
        });
    }

    pub fn set_filter(&mut self, filter: CullingFilter) {
        self.update_scope(false, |s| s.filter = filter);
    }

    pub fn set_storage_tier(&mut self, tier: StorageTier) {
        self.update_scope(false, |s| s.storage_tier = tier);
    }

    pub fn set_sort(&mut self, sort: PhotoSort) {
        self.update_scope(false, |s| s.sort = sort);
    }

    pub fn set_camera(&mut self, camera: Option<String>) {
        self.update_scope(false, |s| s.camera = camera);
    }

    pub fn set_lens(&mut self, lens: Option<String>) {
        self.update_scope(false, |s| s.lens = lens);
    }

    /// Add or remove one derived facet (appended at the end when added).
    pub fn toggle_facet(&mut self, key: &str) {
        self.update_scope(true, |s| toggle(&mut s.facets, key));
    }

    /// Add or remove one colour label (appended at the end when added).
    pub fn toggle_label(&mut self, label: &str) {
        self.update_scope(true, |s| toggle(&mut s.labels, label));
    }

    /// Widen to the whole library, keeping the chosen order.
    ///
    /// For a deep link, whose target may sit outside every active filter: the grid has to
    /// contain the photo before it can be selected. The sort is not a filter — it cannot
    /// hide a row — so it survives. Always advances the query revision (see module docs).
    pub fn clear_scope(&mut self) {
        self.update_scope(true, |s| {
            let sort = s.sort;
            *s = LibraryScope { sort, ..default_scope() };
        });
    }

    // --- the selection -----------------------------------------------------------------

    /// Who the user is acting on.
    pub fn selection(&self) -> LibrarySelection<'_> {
        let photos = self.photos();
        let active = self.active_id.and_then(|active_id| {
            photos
                .iter()
                .find(|p| p.id == active_id)
                .or(self.extra_photo.as_ref().filter(|e| e.id == active_id))
        });
        let selected: HashSet<i64> = self.ids.iter().copied().collect();
        let targets = if !self.ids.is_empty() {
            self.ids.clone()
        } else {
            self.active_id.into_iter().collect()
        };
        LibrarySelection {
            active_id: self.active_id,
            ids: &self.ids,
            active,
            photos: photos.iter().filter(|p| selected.contains(&p.id)).collect(),
            targets,
            extra_photo: self.extra_photo.as_ref(),
            stack_origin: self.stack_origin,
        }
    }

    /// Select with modifier-key semantics: plain = single, Ctrl/Cmd = toggle, Shift = range
    /// from the anchor. Supersedes any off-grid stack-child view.
    pub fn select(&mut self, id: i64, mods: SelectMods) {
        let from = self.anchor.or(self.active_id);
        let range = match from {
            Some(from) if mods.shift => Some(range_between(self.library.photos(), |p| p.id, from, id)),
            _ => None,
        };
        self.apply_select(id, mods, from, range);
    }

    /// [`Self::select`] over an id snapshot instead of the current rows.
    pub fn select_over(&mut self, rows: &[i64], id: i64, mods: SelectMods) {
        let active = self.active_id;
        self.select_over_from(rows, id, mods, active);
    }

    /// [`Self::select_over`] whose Shift origin falls back to `active` rather than the live
    /// active photo. The TS `select` read the anchor from a ref (live) but `activeId` from
    /// its render (captured), so a snapshot step supplies the snapshot's active photo here.
    fn select_over_from(&mut self, rows: &[i64], id: i64, mods: SelectMods, active: Option<i64>) {
        let from = self.anchor.or(active);
        let range = match from {
            Some(from) if mods.shift => Some(range_between(rows, |&r| r, from, id)),
            _ => None,
        };
        self.apply_select(id, mods, from, range);
    }

    /// `range` is `Some` exactly when this is a Shift selection with an origin; its inner
    /// value is `None` when either end is not a row (the selection is then left alone).
    fn apply_select(&mut self, id: i64, mods: SelectMods, from: Option<i64>, range: Option<Option<Vec<i64>>>) {
        if let Some(range) = range {
            self.anchor = from; // pin it, so further Shift steps span the same origin
            if let Some(ids) = range {
                self.ids = ids;
            }
        } else if mods.ctrl {
            if self.ids.contains(&id) {
                self.ids.retain(|&x| x != id);
            } else {
                self.ids.push(id);
            }
            self.anchor = Some(id); // subsequent Shift ranges from this toggle
        } else {
            self.ids = vec![id];
            self.anchor = Some(id);
        }
        self.active_id = Some(id);
        self.extra_photo = None; // a grid selection supersedes an off-grid stack-child view
        self.stack_origin = None;
    }

    /// Make one photo the whole selection and the new Shift anchor, without disturbing an
    /// off-grid view. Keyboard navigation's step; a click should use [`Self::select`].
    pub fn select_single(&mut self, id: i64) {
        self.active_id = Some(id);
        self.ids = vec![id];
        self.anchor = Some(id); // plain navigation resets the Shift anchor
    }

    /// Set the selection without touching the anchor — for a module navigating the shell
    /// (the Tag Graph, the map filmstrip) rather than the user working the grid.
    pub fn select_quiet(&mut self, id: i64) {
        self.active_id = Some(id);
        self.ids = vec![id];
    }

    /// Select every row in the current result, keeping the active photo if there is one.
    pub fn select_all(&mut self) {
        let Some(first) = self.photos().first().map(|p| p.id) else { return };
        self.ids = self.photo_ids();
        let active = self.active_id.unwrap_or(first);
        self.active_id = Some(active);
        self.anchor = Some(active);
    }

    /// Move the active photo `delta` rows through the result, stopping at either end.
    /// `extend` grows the Shift range instead of replacing the selection.
    pub fn step_active(&mut self, delta: isize, extend: bool) {
        let snapshot = self.step_snapshot();
        self.step_active_over(&snapshot, delta, extend);
    }

    /// The rows and the active photo as they are now — what a culling action captures before
    /// it lets a refresh land, then steps over with [`Self::step_active_over`].
    pub fn step_snapshot(&self) -> StepSnapshot {
        StepSnapshot { rows: self.photo_ids(), active_id: self.active_id }
    }

    /// [`Self::step_active`] from a [`StepSnapshot`] taken before a refresh (see module
    /// docs): both the rows and the photo to step from are the snapshot's.
    pub fn step_active_over(&mut self, snapshot: &StepSnapshot, delta: isize, extend: bool) {
        let rows = snapshot.rows.as_slice();
        let Some(active_id) = snapshot.active_id else { return };
        // An off-grid stack child is in no row, so `from` is -1 and a forward step lands on
        // the first row.
        let from = rows.iter().position(|&r| r == active_id).map_or(-1, |i| i as isize);
        let to = from + delta;
        if to < 0 || to as usize >= rows.len() {
            return;
        }
        let next = rows[to as usize];
        if extend {
            self.select_over_from(rows, next, SelectMods::SHIFT, snapshot.active_id);
        } else {
            self.select_single(next);
        }
    }

    /// View any stack member. A row the grid is showing selects normally; a stacked child is
    /// off-grid, so it is held aside and its master remembered for "Back to original".
    pub fn view_photo(&mut self, photo: Photo) {
        let id = photo.id;
        if self.photos().iter().any(|p| p.id == id) {
            self.extra_photo = None;
            self.stack_origin = None;
        } else {
            self.stack_origin = photo.stack_parent_id.or(self.active_id);
            self.extra_photo = Some(photo);
        }
        self.active_id = Some(id);
        self.ids = vec![id];
    }

    /// Return from a stack-child view to the master it is stacked under.
    pub fn back_to_original(&mut self) {
        if let Some(origin) = self.stack_origin.take() {
            self.select(origin, SelectMods::default());
        }
    }

    /// Empty the selection: nothing active, nothing selected, no Shift anchor, and any
    /// off-grid stack-child view dropped. [`Self::reset`]'s selection half, without touching
    /// the scope or the rows.
    pub fn clear_selection(&mut self) {
        self.active_id = None;
        self.ids.clear();
        self.anchor = None;
        self.extra_photo = None;
        self.stack_origin = None;
    }

    /// The active photo's storage badge, which the inspector, loupe and context menu read.
    /// The grid asks for the rows it shows; a photo reached another way — a deep link, a
    /// stack child — never was one. Call after handling input: returns a request when the
    /// active id changed since the last call and is not `None`.
    pub fn take_active_status_request(&mut self) -> Option<StatusRequest> {
        if self.active_id == self.status_effect_for {
            return None;
        }
        self.status_effect_for = self.active_id;
        let id = self.active_id?;
        self.library.request_statuses(&[id])
    }

    // --- lifecycle ---------------------------------------------------------------------

    /// Forget the previous catalog: scope back to the whole library, nothing selected, no
    /// rows, and any request still in flight disowned. Every id here belongs to the catalog
    /// that just closed. Always advances the query revision: the same filters over a
    /// different catalog are a different question.
    pub fn reset(&mut self) {
        self.update_scope(true, |s| *s = default_scope());
        self.clear_selection();
        // Drops the rows immediately and disowns whatever is still in flight.
        self.library.clear();
    }
}

fn toggle(list: &mut Vec<String>, key: &str) {
    if list.iter().any(|k| k == key) {
        list.retain(|k| k != key);
    } else {
        list.push(key.to_string());
    }
}

// Port of `src/modules/__tests__/librarySession.test.tsx` (21 cases, same names, grouped by
// `describe` block). The TS drove the hook with no App mounted; these drive the session the
// same way. Where the TS asserted object identity (`toBe` / `not.toBe`) on `query`,
// `refresh` or `scope`, these assert the matching revision counter.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::test_support::{page, photo};

    fn batch() -> ImportBatch {
        ImportBatch {
            id: 42,
            uuid: "batch-uuid".into(),
            source_label: "/media/card".into(),
            note: String::new(),
            created_at: 0,
            photo_count: 3,
        }
    }

    fn child(id: i64, parent: i64) -> Photo {
        Photo { stack_parent_id: Some(parent), ..photo(id) }
    }

    fn refresh_with(s: &mut LibrarySession, ids: &[i64]) {
        let req = s.refresh();
        let badges = s.apply_page::<()>(&req, Ok(page(ids, ids.len()))).unwrap();
        if let Some(badges) = badges {
            s.apply_statuses::<()>(&badges, Ok(vec![]));
        }
    }

    /// The session with `ids` already listed, as one refresh would leave it.
    fn session_with(ids: &[i64]) -> LibrarySession {
        let mut s = LibrarySession::new();
        refresh_with(&mut s, ids);
        s
    }

    mod a_catalog_switch {
        use super::*;

        #[test]
        fn forgets_the_scope_the_selection_and_the_rows() {
            let mut s = session_with(&[1, 2, 3]);
            s.select_tag(Some(7));
            s.set_filter(CullingFilter::Pick);
            s.set_storage_tier(StorageTier::Nas);
            s.set_sort(PhotoSort::SharpnessAsc);
            s.set_camera(Some("X-T5".into()));
            s.set_lens(Some("XF 35mm".into()));
            s.toggle_facet("has-gps");
            s.toggle_label("Red");
            s.select(2, SelectMods::default());
            s.select(3, SelectMods::CTRL);
            assert_eq!(s.selection().ids, &[2, 3]);
            assert_eq!(s.photos().len(), 3);

            s.reset();

            assert_eq!(s.scope(), &default_scope());
            let sel = s.selection();
            assert_eq!(sel.active_id, None);
            assert!(sel.ids.is_empty());
            assert!(sel.active.is_none());
            assert!(sel.targets.is_empty());
            assert!(sel.extra_photo.is_none());
            assert_eq!(sel.stack_origin, None);
            assert!(s.photos().is_empty());
            assert_eq!(s.total(), 0);
            assert_eq!(s.statuses().len(), 0);
        }

        #[test]
        fn invalidates_the_query_even_when_the_closed_catalog_was_unfiltered() {
            // A view that refetches when the query changes would otherwise show an empty
            // grid forever after switching between two catalogs the user never filtered.
            let mut s = session_with(&[1, 2]);
            let before = s.query();
            let before_revision = s.query_revision();

            s.reset();

            // The TS asserted both `query` and `refresh` changed identity; both are this
            // one revision here.
            assert_ne!(s.query_revision(), before_revision);
            assert_eq!(s.query(), before);
        }

        #[test]
        fn disowns_a_refresh_still_running_against_the_catalog_that_closed() {
            let mut s = session_with(&[1, 2]);
            let switching = s.refresh();
            s.reset();

            s.apply_page::<()>(&switching, Ok(page(&[7, 8], 2))).unwrap();
            assert!(s.photos().is_empty());
        }

        #[test]
        fn forgets_the_shift_anchor_so_the_next_range_starts_fresh() {
            let mut s = session_with(&[1, 2, 3, 4, 5]);
            s.select(2, SelectMods::default()); // anchor ← 2

            s.reset();
            refresh_with(&mut s, &[1, 2, 3, 4, 5]); // the new catalog lists the same ids

            // With the anchor still at 2 this would select 2..4; the switch dropped it, so a
            // Shift click with nothing active is an ordinary single selection.
            s.select(4, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[4]);
        }
    }

    mod the_scope {
        use super::*;

        #[test]
        fn keeps_the_four_sidebar_scopes_mutually_exclusive() {
            let mut s = LibrarySession::new();

            s.select_tag(Some(7));
            assert_eq!(s.scope().tag_id, Some(7));

            s.select_album(Some(3));
            let sc = s.scope();
            assert_eq!((sc.album_id, sc.tag_id, sc.batch_id, sc.smart_album_id), (Some(3), None, None, None));
            assert!(sc.batch.is_none());

            s.select_batch(Some(batch()));
            let sc = s.scope();
            assert_eq!((sc.batch_id, sc.tag_id, sc.album_id, sc.smart_album_id), (Some(42), None, None, None));
            assert!(batch_eq(sc.batch.as_ref().unwrap(), &batch()));

            s.select_smart_album(Some(5));
            let sc = s.scope();
            assert_eq!((sc.smart_album_id, sc.tag_id, sc.album_id, sc.batch_id), (Some(5), None, None, None));
            assert!(sc.batch.is_none());

            s.select_tag(Some(9));
            assert_eq!((s.scope().tag_id, s.scope().smart_album_id), (Some(9), None));
        }

        #[test]
        fn leaves_the_other_scopes_alone_when_one_is_cleared() {
            let mut s = LibrarySession::new();
            s.select_tag(Some(7));
            s.select_album(None); // the filter bar's "clear album" chip
            assert_eq!(s.scope().tag_id, Some(7));
            assert_eq!(s.scope().album_id, None);
        }

        #[test]
        fn derives_the_backend_query_from_the_scope() {
            let mut s = LibrarySession::new();
            s.select_batch(Some(batch()));
            s.set_filter(CullingFilter::Reject);
            s.set_sort(PhotoSort::SharpnessDesc);
            s.toggle_facet("has-gps");
            s.toggle_label("Blue");
            assert_eq!(
                s.query(),
                PhotoQuery {
                    tag_id: None,
                    album_id: None,
                    batch_id: Some(42),
                    smart_album_id: None,
                    facets: vec!["has-gps".into()],
                    culling_filter: CullingFilter::Reject,
                    storage_tier: StorageTier::All,
                    camera: None,
                    lens: None,
                    labels: vec!["Blue".into()],
                    sort: PhotoSort::SharpnessDesc,
                    window: None,
                }
            );
            // `batch` is the session's, not the query's — the backend filters on the id.
            let json = serde_json::to_value(s.query()).unwrap();
            assert!(!json.as_object().unwrap().contains_key("batch"));
        }

        #[test]
        fn does_not_invalidate_the_query_when_the_value_is_already_current() {
            let mut s = LibrarySession::new();
            s.set_filter(CullingFilter::Pick);
            let picked = s.query_revision();

            // Clicking the chip you are already on, or the tag that is already the scope: a
            // new query here would refetch the whole library for nothing.
            s.set_filter(CullingFilter::Pick);
            assert_eq!(s.query_revision(), picked);
            s.select_tag(None);
            assert_eq!(s.query_revision(), picked);

            // Re-picking the batch that is already the scope, from a panel that has since
            // reloaded its list: an equal-but-new row is the same question for the backend.
            s.select_batch(Some(batch()));
            let batched = s.query_revision();
            s.select_batch(Some(batch()));
            assert_eq!(s.query_revision(), batched);

            s.set_filter(CullingFilter::Reject);
            assert_ne!(s.query_revision(), batched);
        }

        #[test]
        fn toggles_facets_and_labels_on_and_off() {
            let mut s = LibrarySession::new();
            s.toggle_facet("has-gps");
            s.toggle_facet("published:flickr");
            assert_eq!(s.scope().facets, vec!["has-gps", "published:flickr"]);
            s.toggle_facet("has-gps");
            assert_eq!(s.scope().facets, vec!["published:flickr"]);

            s.toggle_label("");
            assert_eq!(s.scope().labels, vec![""]);
            s.toggle_label("");
            assert!(s.scope().labels.is_empty());
        }

        #[test]
        fn widens_to_the_whole_library_for_a_deep_link_keeping_the_chosen_order() {
            let mut s = LibrarySession::new();
            s.select_tag(Some(7));
            s.set_filter(CullingFilter::Pick);
            s.set_storage_tier(StorageTier::Local);
            s.set_camera(Some("X-T5".into()));
            s.toggle_facet("has-gps");
            s.toggle_label("Red");
            s.set_sort(PhotoSort::SharpnessAsc);

            s.clear_scope();

            // Nothing can hide the linked photo any more…
            assert_eq!(s.scope(), &LibraryScope { sort: PhotoSort::SharpnessAsc, ..default_scope() });
            // …but the order is not a filter, so following a link does not silently re-sort
            // the grid the user was working in. A catalog switch, which does, is `reset`.
            assert_eq!(s.scope().sort, PhotoSort::SharpnessAsc);
        }
    }

    mod the_selection {
        use super::*;

        #[test]
        fn replaces_on_a_plain_click_toggles_on_ctrl_and_ranges_on_shift() {
            let mut s = session_with(&[1, 2, 3, 4, 5]);

            s.select(2, SelectMods::default());
            assert_eq!(s.selection().ids, &[2]);
            assert_eq!(s.selection().active_id, Some(2));

            s.select(4, SelectMods::CTRL);
            assert_eq!(s.selection().ids, &[2, 4]);
            s.select(4, SelectMods::CTRL);
            assert_eq!(s.selection().ids, &[2]);
            assert_eq!(s.selection().active_id, Some(4)); // still the one last clicked

            // The Ctrl-click pinned the anchor at 4, so the range spans 4↔1 — backwards, and
            // inclusive of both ends.
            s.select(1, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[1, 2, 3, 4]);

            // Extending again spans the same origin rather than walking the anchor along.
            s.select(5, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[4, 5]);
        }

        #[test]
        fn selects_every_row_keeping_the_active_photo_as_the_anchor() {
            let mut s = session_with(&[1, 2, 3]);
            s.select(2, SelectMods::default());
            s.select_all();
            assert_eq!(s.selection().ids, &[1, 2, 3]);
            assert_eq!(s.selection().active_id, Some(2));

            // The anchor moved with select-all, so a following Shift click ranges from the
            // active photo, not from wherever the last range ended.
            s.select(3, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[2, 3]);
        }

        #[test]
        fn selects_the_first_row_when_select_all_runs_with_nothing_active() {
            let mut s = session_with(&[4, 5, 6]);
            s.select_all();
            assert_eq!(s.selection().active_id, Some(4));
            assert_eq!(s.selection().ids, &[4, 5, 6]);
        }

        #[test]
        fn steps_the_active_photo_through_the_result_and_stops_at_both_ends() {
            let mut s = session_with(&[1, 2, 3]);
            s.select(1, SelectMods::default());

            s.step_active(1, false);
            assert_eq!(s.selection().active_id, Some(2));
            s.step_active(1, false);
            assert_eq!(s.selection().active_id, Some(3));
            s.step_active(1, false); // already the last row
            assert_eq!(s.selection().active_id, Some(3));

            s.step_active(-1, true); // Shift+Arrow extends
            assert_eq!(s.selection().active_id, Some(2));
            assert_eq!(s.selection().ids, &[2, 3]);

            s.select_single(1);
            s.step_active(-1, false); // already the first row
            assert_eq!(s.selection().active_id, Some(1));
            assert_eq!(s.selection().ids, &[1]);
        }

        #[test]
        fn views_a_stacked_child_the_grid_never_listed_and_comes_back() {
            let mut s = session_with(&[1, 2, 3]);
            s.select(2, SelectMods::default());

            s.view_photo(child(99, 2));
            let sel = s.selection();
            assert_eq!(sel.active_id, Some(99));
            assert_eq!(sel.extra_photo.map(|p| (p.id, p.stack_parent_id)), Some((99, Some(2))));
            assert_eq!(sel.stack_origin, Some(2));
            // The active photo resolves even though no row holds it.
            assert_eq!(sel.active.map(|p| p.id), Some(99));
            // …and it is what a bulk action would apply to.
            assert_eq!(sel.targets, vec![99]);

            s.back_to_original();
            let sel = s.selection();
            assert_eq!(sel.active_id, Some(2));
            assert!(sel.extra_photo.is_none());
            assert_eq!(sel.stack_origin, None);
            assert_eq!(sel.active.map(|p| p.id), Some(2));
        }

        #[test]
        fn clears_the_off_grid_view_when_the_grid_is_selected_again() {
            let mut s = session_with(&[1, 2, 3]);
            s.view_photo(child(99, 1));
            s.select(3, SelectMods::default());
            assert!(s.selection().extra_photo.is_none());
            assert_eq!(s.selection().stack_origin, None);

            // A row the grid *is* showing is an ordinary selection, not an off-grid view.
            s.view_photo(photo(1));
            assert!(s.selection().extra_photo.is_none());
            assert_eq!(s.selection().active_id, Some(1));
        }

        #[test]
        fn navigates_without_moving_the_anchor_for_a_module_driving_the_shell() {
            let mut s = session_with(&[1, 2, 3, 4]);
            s.select(1, SelectMods::default()); // anchor ← 1
            s.select_quiet(3);
            assert_eq!(s.selection().ids, &[3]);

            // The anchor is still where the user left it, so a Shift click spans 1↔4.
            s.select(4, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[1, 2, 3, 4]);
        }

        #[test]
        fn falls_back_to_the_active_photo_when_nothing_is_selected() {
            let mut s = session_with(&[1, 2, 3]);
            assert!(s.selection().targets.is_empty());

            s.select(2, SelectMods::default());
            s.select(2, SelectMods::CTRL); // toggled the only one back off
            assert!(s.selection().ids.is_empty());
            assert_eq!(s.selection().targets, vec![2]);
        }

        #[test]
        fn exposes_the_selected_rows_for_the_host_bridge() {
            let mut s = session_with(&[1, 2, 3]);
            s.select(1, SelectMods::default());
            s.select(3, SelectMods::CTRL);
            let ids: Vec<i64> = s.selection().photos.iter().map(|p| p.id).collect();
            assert_eq!(ids, vec![1, 3]);
        }

        #[test]
        fn asks_for_the_active_photo_s_storage_badge_wherever_it_came_from() {
            let mut s = session_with(&[1, 2, 3]);
            s.select(2, SelectMods::default());
            assert_eq!(s.take_active_status_request().map(|r| r.ids), Some(vec![2]));

            // Including a stack child, which is in no row the grid ever reported.
            s.view_photo(child(99, 2));
            assert_eq!(s.take_active_status_request().map(|r| r.ids), Some(vec![99]));
        }

        #[test]
        fn clears_the_selection_without_touching_the_scope_or_the_rows() {
            // The bench's ✕ — reset()'s selection half. Everything selection-scoped goes; the
            // scope, the derived query and the rows stay exactly as they were.
            let mut s = session_with(&[1, 2, 3]);
            s.select_tag(Some(7));
            s.select(2, SelectMods::default());
            s.select(3, SelectMods::CTRL);
            s.view_photo(child(99, 2));
            let scope_before = s.scope_revision();
            let query_before = s.query_revision();

            s.clear_selection();

            let sel = s.selection();
            assert_eq!(sel.active_id, None);
            assert!(sel.ids.is_empty());
            assert!(sel.active.is_none());
            assert!(sel.targets.is_empty());
            assert!(sel.extra_photo.is_none());
            assert_eq!(sel.stack_origin, None);
            // Unlike reset(): same scope, same query (no refetch), rows kept.
            assert_eq!(s.scope_revision(), scope_before);
            assert_eq!(s.query_revision(), query_before);
            assert_eq!(s.photos().len(), 3);

            // The Shift anchor went with the selection, so — with nothing active — a Shift
            // click afterwards is an ordinary single selection, not a range from 2.
            s.select(3, SelectMods::SHIFT);
            assert_eq!(s.selection().ids, &[3]);
        }
    }

    // New (not in the vitest file): the stale-row step the culling shortcut relies on,
    // which the TS got from closures and the port exposes as `step_active_over`.
    #[test]
    fn steps_over_the_rows_it_started_from_after_a_refresh_drops_the_active_photo() {
        let mut s = session_with(&[1, 2, 3]);
        s.select(2, SelectMods::default());
        let before = s.step_snapshot();
        // Rating 2 as a reject under a "pick" filter drops it from the refreshed rows.
        refresh_with(&mut s, &[1, 3]);
        s.step_active_over(&before, 1, false);
        assert_eq!(s.selection().active_id, Some(3));
        // Over the refreshed rows, 2 is in no row: a forward step lands on the first one.
        s.select_quiet(2);
        s.step_active(1, false);
        assert_eq!(s.selection().active_id, Some(1));
    }

    // New (#158 review, M1/M2): a page that no longer lists a selected photo — a filter hid
    // it, it was trashed or removed — unselects it, so no bulk action reaches it. A stale page
    // trims nothing; the off-grid stack child stays.
    #[test]
    fn a_landed_page_unselects_the_photos_it_no_longer_lists() {
        let mut s = session_with(&[1, 2, 3, 4]);
        s.select(1, SelectMods::default());
        s.select(2, SelectMods::CTRL);
        s.select(3, SelectMods::CTRL);
        let stale = s.refresh();
        refresh_with(&mut s, &[1, 4]);
        assert_eq!(s.selection().ids, &[1]);
        assert_eq!(s.selection().active_id, None, "the active photo (3) is hidden");
        assert_eq!(s.selection().targets, vec![1]);
        // A stale page changes nothing (it is dropped).
        s.select(4, SelectMods::CTRL);
        s.apply_page::<()>(&stale, Ok(page(&[2], 1))).unwrap();
        assert_eq!(s.selection().ids, &[1, 4]);

        // Nothing left: no targets at all.
        refresh_with(&mut s, &[2]);
        assert!(s.selection().ids.is_empty() && s.selection().targets.is_empty());

        // The loupe's off-grid stack child is kept.
        s.view_photo(child(9, 2));
        refresh_with(&mut s, &[2]);
        assert_eq!(s.selection().active_id, Some(9));
    }

    // New (batch 7 review, L1): `trim_selection_to_rows` trims the Shift anchor too, not only
    // `ids`/`active_id` — untested before, so a regression here passed every other test.
    #[test]
    fn a_landed_page_trims_a_hidden_shift_anchor_too() {
        let mut s = session_with(&[1, 2, 3, 4, 5]);
        s.select(1, SelectMods::default()); // anchor <- 1
        s.select(3, SelectMods::SHIFT); // anchor pinned at 1; range 1..3
        assert_eq!(s.selection().ids, &[1, 2, 3]);

        // A filter hides photo 1 — the anchor, not just a plain selected id.
        refresh_with(&mut s, &[2, 3, 4, 5]);
        assert_eq!(s.selection().ids, &[2, 3], "1 left the selection along with the row");

        // With the anchor trimmed, Shift now ranges from the active photo (3) to 5. Kept, the
        // anchor (1) is absent from the rows, range_between finds no position for it, and the
        // selection is left exactly as the trim above made it — no [3, 4, 5] range at all.
        s.select(5, SelectMods::SHIFT);
        assert_eq!(s.selection().ids, &[3, 4, 5], "ranges from the active photo, the anchor having been dropped");
    }

    // New (Codex review of gpui #102): two culling actions started on the same photo both
    // step from it, as the TS closures did — the second must not advance past an unjudged
    // photo because the first already moved the selection.
    #[test]
    fn overlapping_culls_started_on_one_photo_both_step_from_it() {
        let mut s = session_with(&[1, 2, 3]);
        s.select(1, SelectMods::default());
        let first = s.step_snapshot();
        let second = s.step_snapshot();
        s.step_active_over(&first, 1, false);
        assert_eq!(s.selection().active_id, Some(2));
        s.step_active_over(&second, 1, false);
        assert_eq!(s.selection().active_id, Some(2), "must not skip to 3");
    }

    // New (Codex review of 891b85a): a Shift step from a snapshot ranges from the snapshot's
    // active photo when there is no live anchor, as the TS closure's captured `activeId` did.
    #[test]
    fn an_extended_snapshot_step_ranges_from_the_snapshots_active_photo() {
        let mut s = session_with(&[1, 2, 3]);
        s.select(1, SelectMods::default());
        let snapshot = s.step_snapshot();
        s.clear_selection(); // no live anchor, nothing active
        s.step_active_over(&snapshot, 1, true);
        assert_eq!(s.selection().ids, &[1, 2]);
        assert_eq!(s.selection().active_id, Some(2));
    }
}
