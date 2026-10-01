//! [`ShellState`]: what the shell chrome shows and edits — the layout (column visibility
//! and widths, thumbnail size, open sections), the active surface, the Library session
//! (scope and selection, `chairphoto_model::library::session`), and the catalog-derived
//! lists and counts the title bar, command pill, bench and collection browser read.
//!
//! **Data flow.** As `model.rs` describes: this entity is the one owner of each list it
//! holds, refetched off the UI thread whenever the [`AppModel`] has re-read the catalog
//! ([`AppModelEvent::CatalogRead`] — after startup's open, a `catalog:switched`, a scan's
//! `done`: the model's own invalidation rules, not a second copy of them), and — for the
//! match count and the scope chips' names — whenever the Library scope changes. A
//! `catalog:switched` first drops everything that named the old catalog. Each read carries a
//! generation; a result a newer read superseded is dropped.
//!
//! **Not persisted yet.** React kept the layout in localStorage (`panel.leftW`,
//! `panel.rightW`, `panel.leftHidden`, `panel.rightHidden`, `panel.thumbSize`,
//! `panel.inspectorTab`, `panel.section.*`). The GPUI app has no per-machine settings store
//! yet (the same gap as the appearance mode, `theme/mod.rs`; Preferences, #113), so these
//! start at React's defaults each launch.

use crate::model::{AppModel, AppModelEvent, DeepLinkTarget};
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use chairphoto_core::catalog::{
    Album, Catalog, Facet, ImportBatch, Photo, PhotoPage, PhotoQuery, PickState, SmartAlbum,
    SOFT_THRESHOLD_DEFAULT, SOFT_THRESHOLD_KEY,
};
use chairphoto_model::deep_link::DeepLinkView;
use chairphoto_model::library::query::{RefreshRequest, StatusRequest};
use chairphoto_model::library::session::{LibrarySession, SelectMods};
use gpui_kit::{Context, Entity, EventEmitter, Subscription, Task};

/// React's column defaults and drag limits (`App.tsx`).
pub const LEFT_DEFAULT_W: f32 = 210.;
pub const RIGHT_DEFAULT_W: f32 = 316.;
pub const COLUMN_MIN_W: f32 = 140.;
pub const COLUMN_MAX_W: f32 = 640.;
/// The thumbnail-size slider (`panel.thumbSize`): 120–320 in steps of 8, default 160.
pub const THUMB_MIN: f32 = 120.;
pub const THUMB_MAX: f32 = 320.;
pub const THUMB_STEP: f32 = 8.;
pub const THUMB_DEFAULT: f32 = 160.;
/// At or below this window width the side columns become overlays (`useNarrow.ts`).
pub const NARROW_MAX_W: f32 = 1024.;

/// Which side column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// The column layout. Narrow windows keep `left_hidden`/`right_hidden` untouched and use
/// the transient overlays instead, as React did, so shrinking the window never clobbers
/// the desktop preference.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub left_hidden: bool,
    pub right_hidden: bool,
    pub left_w: f32,
    pub right_w: f32,
    /// Narrow: whether each overlay is open. Never persisted.
    pub overlay_left: bool,
    pub overlay_right: bool,
    pub thumb_size: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            left_hidden: false,
            right_hidden: false,
            left_w: LEFT_DEFAULT_W,
            right_w: RIGHT_DEFAULT_W,
            overlay_left: false,
            overlay_right: false,
            thumb_size: THUMB_DEFAULT,
        }
    }
}

/// Clamp a dragged column width to React's 140–640 px.
pub fn clamp_column(w: f32) -> f32 {
    w.clamp(COLUMN_MIN_W, COLUMN_MAX_W)
}

/// Snap a slider value to the thumbnail sizes React's `<input type=range step=8>` allowed.
pub fn snap_thumb(v: f32) -> f32 {
    let snapped = THUMB_MIN + ((v - THUMB_MIN) / THUMB_STEP).round() * THUMB_STEP;
    snapped.clamp(THUMB_MIN, THUMB_MAX)
}

/// What the stage shows: the icon rail's active item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    Library,
    /// Reserved for the Darkroom (#111); the rail's Develop is not ported yet.
    Develop,
    /// A module's main view (`modules::MainView`), by view id.
    Module(String),
}

/// The inspector's tabs, in order (`INSPECTOR_TABS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorTab {
    Details,
    Tags,
    Versions,
    Publish,
}

impl InspectorTab {
    pub const ALL: [InspectorTab; 4] =
        [InspectorTab::Details, InspectorTab::Tags, InspectorTab::Versions, InspectorTab::Publish];

    pub fn label(self) -> &'static str {
        match self {
            InspectorTab::Details => "details",
            InspectorTab::Tags => "tags",
            InspectorTab::Versions => "versions",
            InspectorTab::Publish => "publish",
        }
    }
}

/// The collection browser's collapsible sections (`panel.section.*`), open by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Tags,
    SmartAlbums,
    Albums,
    Batches,
}

impl Section {
    pub const ALL: [Section; 4] = [Section::Tags, Section::SmartAlbums, Section::Albums, Section::Batches];

    pub fn label(self) -> &'static str {
        match self {
            Section::Tags => "tags",
            Section::SmartAlbums => "smart albums",
            Section::Albums => "albums",
            Section::Batches => "import batches",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A background job's readout for the bench (`benchProgress` in App.tsx).
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub label: String,
    pub done: usize,
    /// `None` = indeterminate: the bar shows a fixed 40 % fill.
    pub total: Option<usize>,
}

/// The three jobs the bench reports, as their last events left them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Jobs {
    /// `import:progress` of [`Self::import_job`] — cleared by the import flow when it ends
    /// (Storage and import, #114) and on a catalog switch.
    pub import: Option<(usize, usize)>,
    /// The import job the bench follows, set by the import flow when it claims one. Progress
    /// from any other job — a superseded import, or one from the catalog that was left — is a
    /// straggler and moves nothing, so it cannot put the bench back on a dead import.
    pub import_job: Option<u64>,
    /// `scan:progress` — cleared by its `done` phase.
    pub scan: Option<(String, usize, usize)>,
    /// `develop:progress` — `(phase, editor)`, cleared by done/nochange/error.
    pub develop: Option<(String, String)>,
}

impl Jobs {
    /// The one job worth showing: import, then scan, then develop — React's labels.
    pub fn bench_progress(&self) -> Option<Progress> {
        if let Some((done, total)) = self.import {
            let label = if total > 0 { format!("Importing {done}/{total}") } else { "Importing …".into() };
            return Some(Progress { label, done, total: (total > 0).then_some(total) });
        }
        if let Some((phase, done, total)) = &self.scan {
            let label = match phase.as_str() {
                "metadata" => format!(
                    "Reading metadata {}/{}",
                    crate::shell::style::grouped(*done),
                    crate::shell::style::grouped(*total)
                ),
                "finalizing" => "Finalizing…".into(),
                _ => format!("Indexing {}…", crate::shell::style::grouped(*done)),
            };
            return Some(Progress { label, done: *done, total: (*total > 0).then_some(*total) });
        }
        if let Some((phase, editor)) = &self.develop {
            let label =
                if phase == "rendering" { format!("Rendering {editor}…") } else { format!("Editing in {editor}…") };
            return Some(Progress { label, done: 0, total: None });
        }
        None
    }

    /// Fold one core event in. Returns whether anything changed.
    pub fn on_core_event(&mut self, event: &CoreEvent) -> bool {
        match event {
            CoreEvent::ImportProgress(p) if self.import_job == Some(p.job) => self.import = Some((p.done, p.total)),
            CoreEvent::ScanProgress(p) if p.phase == "done" => self.scan = None,
            CoreEvent::ScanProgress(p) => self.scan = Some((p.phase.clone(), p.done, p.total)),
            CoreEvent::DevelopProgress(p) if matches!(p.phase.as_str(), "done" | "nochange" | "error") => {
                self.develop = None
            }
            CoreEvent::DevelopProgress(p) => self.develop = Some((p.phase.clone(), p.editor.clone())),
            CoreEvent::CatalogSwitched(_) => *self = Jobs::default(),
            _ => return false,
        }
        true
    }
}

/// The catalog-derived lists the menus and chips offer.
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub facets: Vec<Facet>,
    pub cameras: Vec<String>,
    pub lenses: Vec<String>,
    /// Import batches, newest first as the catalog lists them — the "Export a bundle"
    /// submenu.
    pub batches: Vec<ImportBatch>,
    pub albums: Vec<Album>,
    pub smart_albums: Vec<SmartAlbum>,
}

/// Counts behind the attention chips and the Trash row. `None` = unknown (not read yet,
/// or the last read failed): the chip shows `?` and the Trash row hides its count, rather
/// than a stale or misleading number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counts {
    /// Actionable (status `pending`) back-up operations. A failed read counts 0, as React.
    pub pending: usize,
    pub identity_debt: Option<i64>,
    pub trash: Option<usize>,
}

/// What the current scope resolves to: the match count and the chips' names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScopeInfo {
    /// Photos matching the current query; `None` until read (the bench then falls back to
    /// the catalog's total).
    pub total: Option<usize>,
    pub tag_name: Option<String>,
    pub album_name: Option<String>,
    pub smart_album_name: Option<String>,
}

/// A culling mark — what the Library's keys (0–5, P/X/U, R/Y/G/B/V/N) and the bench's
/// marking controls write. Both go through [`ShellState::apply_mark`], the one write path
/// (App.tsx's `applyToSelection`), so the two surfaces cannot drift apart.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// Stars, 0–5 (0 clears).
    Rating(i64),
    Pick(PickState),
    /// A colour label's stored name; `""` clears it.
    Label(String),
}

impl Mark {
    /// Write the mark on one photo: the Tauri `set_rating` / `set_pick_state` /
    /// `set_label` commands' body (`Catalog::set_culling`).
    fn write(&self, c: &Catalog, photo_id: i64) -> chairphoto_core::catalog::Result<Photo> {
        match self {
            Mark::Rating(r) => c.set_culling(photo_id, Some(*r), None, None),
            Mark::Pick(p) => c.set_culling(photo_id, None, None, Some(*p)),
            Mark::Label(l) => c.set_culling(photo_id, None, Some(l), None),
        }
    }
}

/// A `chairphoto://<uuid>` link waiting for the widened grid to list its photo (App.tsx's
/// `deepLinkTarget`).
#[derive(Debug, Clone)]
pub struct PendingPhotoLink {
    pub photo: Photo,
    pub view: DeepLinkView,
}

/// The shell's state. See the module docs.
pub struct ShellState {
    app: AppState,
    pub layout: Layout,
    /// The width the window had at the last render; decides the narrow layout.
    pub narrow: bool,
    pub surface: Surface,
    pub library: LibrarySession,
    /// Import ▾ → "Cache previews on import": session-only, default on (App.tsx).
    pub cache_previews: bool,
    pub inspector_tab: InspectorTab,
    sections_open: [bool; 4],
    pub jobs: Jobs,
    pub lists: Lists,
    pub counts: Counts,
    pub scope_info: ScopeInfo,
    /// `sharpness.soft_threshold`: below it a tile shows the soft `~` badge.
    pub soft_threshold: f64,
    /// Whether the current query's rows have landed — until then the grid shows no
    /// "No photos" empty state, which would read as an answer.
    pub rows_loaded: bool,
    /// The generation of the row read still in flight, if any.
    rows_pending: Option<u64>,
    /// A photo link waiting for the grid to list its photo.
    pub pending_link: Option<PendingPhotoLink>,
    /// The newest culling write: each write waits for the one before, so marks land in the
    /// order they were made.
    last_mark: Option<Task<()>>,
    /// Bumped by every `catalog:switched`: a mark or burst analysis queued before it names
    /// photos of the catalog that closed and must not be written.
    catalog_generation: u64,
    model: Entity<AppModel>,
    lists_generation: u64,
    scope_generation: u64,
    /// Owns `counts.pending`/`counts.trash`: bumped by every read that writes them (a catalog
    /// refresh and a focus refresh alike) and by a catalog switch, so only the newest read's
    /// counts land (Codex review of e06d3c5).
    counts_generation: u64,
    _model_events: Subscription,
}

/// Emitted when the Library scope changed, so the Library view (#106) can re-run its query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeChanged {
    pub query_revision: u64,
}

impl EventEmitter<ScopeChanged> for ShellState {}

impl ShellState {
    pub fn new(model: &Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let app = model.read(cx).state().clone();
        let _model_events = cx.subscribe(model, |this, _, event: &AppModelEvent, cx| match event {
            AppModelEvent::CatalogRead => this.refresh_catalog_data(cx),
            AppModelEvent::Core(event) => this.on_core_event(event, cx),
            AppModelEvent::DeepLink(target) => this.apply_deep_link(target.clone(), cx),
        });
        ShellState {
            app,
            layout: Layout::default(),
            narrow: false,
            surface: Surface::Library,
            library: LibrarySession::new(),
            cache_previews: true,
            inspector_tab: InspectorTab::Details,
            sections_open: [true; 4],
            jobs: Jobs::default(),
            lists: Lists::default(),
            counts: Counts::default(),
            scope_info: ScopeInfo::default(),
            soft_threshold: SOFT_THRESHOLD_DEFAULT,
            rows_loaded: false,
            rows_pending: None,
            pending_link: None,
            last_mark: None,
            catalog_generation: 0,
            model: model.clone(),
            lists_generation: 0,
            scope_generation: 0,
            counts_generation: 0,
            _model_events,
        }
    }

    // --- layout ----------------------------------------------------------------------

    /// `[` and More ⋯ → View → "Tags & collections panel". Narrow: the overlay.
    pub fn toggle_panel(&mut self, side: Side, cx: &mut Context<Self>) {
        let l = &mut self.layout;
        match (side, self.narrow) {
            (Side::Left, true) => l.overlay_left = !l.overlay_left,
            (Side::Left, false) => l.left_hidden = !l.left_hidden,
            (Side::Right, true) => l.overlay_right = !l.overlay_right,
            (Side::Right, false) => l.right_hidden = !l.right_hidden,
        }
        cx.notify();
    }

    /// Hide one column: the inspector's own hide button, or a click on a narrow overlay's
    /// scrim (which only closes the overlay).
    pub fn hide_panel(&mut self, side: Side, cx: &mut Context<Self>) {
        let l = &mut self.layout;
        match (side, self.narrow) {
            (Side::Left, true) => l.overlay_left = false,
            (Side::Left, false) => l.left_hidden = true,
            (Side::Right, true) => l.overlay_right = false,
            (Side::Right, false) => l.right_hidden = true,
        }
        cx.notify();
    }

    /// Whether a column is showing — what the View menu's checkboxes read.
    pub fn panel_visible(&self, side: Side) -> bool {
        match (side, self.narrow) {
            (Side::Left, true) => self.layout.overlay_left,
            (Side::Left, false) => !self.layout.left_hidden,
            (Side::Right, true) => self.layout.overlay_right,
            (Side::Right, false) => !self.layout.right_hidden,
        }
    }

    /// Record the window's width class. Leaving narrow closes both overlays, so a later
    /// narrow session starts closed (React's effect on `narrow`).
    pub fn set_narrow(&mut self, narrow: bool) {
        if self.narrow != narrow {
            self.narrow = narrow;
            if !narrow {
                self.layout.overlay_left = false;
                self.layout.overlay_right = false;
            }
        }
    }

    pub fn set_column_width(&mut self, side: Side, w: f32, cx: &mut Context<Self>) {
        let w = clamp_column(w);
        match side {
            Side::Left => self.layout.left_w = w,
            Side::Right => self.layout.right_w = w,
        }
        cx.notify();
    }

    pub fn set_thumb_size(&mut self, v: f32, cx: &mut Context<Self>) {
        let v = snap_thumb(v);
        if self.layout.thumb_size != v {
            self.layout.thumb_size = v;
            cx.notify();
        }
    }

    pub fn section_open(&self, section: Section) -> bool {
        self.sections_open[section.index()]
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let open = &mut self.sections_open[section.index()];
        *open = !*open;
        cx.notify();
    }

    pub fn set_inspector_tab(&mut self, tab: InspectorTab, cx: &mut Context<Self>) {
        self.inspector_tab = tab;
        cx.notify();
    }

    pub fn toggle_cache_previews(&mut self, cx: &mut Context<Self>) {
        self.cache_previews = !self.cache_previews;
        cx.notify();
    }

    pub fn show_library(&mut self, cx: &mut Context<Self>) {
        self.surface = Surface::Library;
        cx.notify();
    }

    /// A module main view's rail item: the stage shows that view.
    pub fn show_module_view(&mut self, view_id: &str, cx: &mut Context<Self>) {
        self.surface = Surface::Module(view_id.to_string());
        cx.notify();
    }

    // --- the Library scope and selection ------------------------------------------------

    /// Run one of the session's scope verbs; when the derived query changed, re-read the
    /// match count and chip names, and tell the Library view.
    pub fn update_scope(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut LibrarySession)) {
        let before = self.library.query_revision();
        f(&mut self.library);
        let after = self.library.query_revision();
        if after != before {
            self.refresh_scope(cx);
            self.refresh_rows(cx);
            cx.emit(ScopeChanged { query_revision: after });
        }
        cx.notify();
    }

    /// No tag/album/batch/smart-album scope: the collection browser's "All photos" is on.
    pub fn is_all_scope(&self) -> bool {
        let s = self.library.scope();
        s.tag_id.is_none() && s.album_id.is_none() && s.batch_id.is_none() && s.smart_album_id.is_none()
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.library.clear_selection();
        cx.notify();
    }

    /// Run a selection verb (a click, a key), then ask for the active photo's storage badge
    /// if it changed — what React's effect on the active id did after a commit.
    pub fn select_with(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut LibrarySession)) {
        f(&mut self.library);
        self.after_input(cx);
    }

    fn after_input(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.library.take_active_status_request() {
            self.fetch_statuses(request, cx);
        }
        cx.notify();
    }

    // --- the Library's rows ----------------------------------------------------------

    /// Re-run the current query off the UI thread (`list_photos`). Only the newest read
    /// lands: the session drops a page whose generation is stale.
    pub fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let request = self.library.refresh();
        self.rows_pending = Some(request.generation);
        let state = self.app.clone();
        let query = request.query.clone();
        let read = cx.background_executor().spawn(async move { with_catalog(&state, |c| c.photo_page(&query)) });
        cx.spawn(async move |this, cx| {
            let page = read.await;
            this.update(cx, |s, cx| s.on_page(&request, page, cx)).ok();
        })
        .detach();
    }

    pub(crate) fn on_page(&mut self, request: &RefreshRequest, page: Result<PhotoPage, String>, cx: &mut Context<Self>) {
        let landed = self.rows_pending == Some(request.generation);
        match self.library.apply_page(request, page) {
            Ok(statuses) => {
                if let Some(statuses) = statuses {
                    self.fetch_statuses(statuses, cx);
                }
                if landed {
                    self.rows_pending = None;
                    self.rows_loaded = true;
                    self.apply_pending_link(cx);
                    self.after_input(cx);
                }
            }
            // The rows stay as they were: an empty grid would read as "no photos match".
            Err(e) if landed => {
                self.rows_pending = None;
                eprintln!("library: rows unavailable: {e}");
                self.model.update(cx, |m, cx| m.set_status(format!("Could not list photos: {e}"), cx));
            }
            Err(_) => {}
        }
        cx.notify();
    }

    /// The grid's on-screen rows (plus overscan), as a half-open range over the rows: their
    /// storage badges are fetched, and only theirs (`LibraryQuery::set_visible_range`).
    pub fn set_visible_range(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        if let Some(request) = self.library.set_visible_range(start, end) {
            self.fetch_statuses(request, cx);
        }
    }

    fn fetch_statuses(&mut self, request: StatusRequest, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let ids = request.ids.clone();
        let read = cx
            .background_executor()
            .spawn(async move { chairphoto_core::app::photo_storage_statuses(&state, &ids) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |s, cx| {
                s.library.apply_statuses(&request, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- culling marks ---------------------------------------------------------------

    /// The one write path for culling marks (App.tsx's `applyToSelection`): write `mark` on
    /// every targeted photo (the selection, else the active photo), then re-read the rows.
    ///
    /// `advance` is the keyboard's own behaviour: when exactly one photo was marked, step
    /// to the next one — over the rows as they were when the key was pressed, not the
    /// refreshed ones the photo may have been filtered out of (`step_active_over`). The
    /// bench's controls pass `false`: clicking a star must not move the selection.
    ///
    /// Writes run off the UI thread, one after another in the order they were made. A
    /// write still queued at a catalog switch is dropped: its ids name the closed catalog's
    /// photos. (One already running when the switch lands cannot be recalled; the switch
    /// itself is Storage and import, #114.)
    pub fn apply_mark(&mut self, mark: Mark, advance: bool, cx: &mut Context<Self>) {
        let targets = self.library.selection().targets;
        if targets.is_empty() {
            return;
        }
        let snapshot = (advance && targets.len() == 1).then(|| self.library.step_snapshot());
        let previous = self.last_mark.take();
        let generation = self.catalog_generation;
        let state = self.app.clone();
        self.last_mark = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let current = this.update(cx, |s, _| s.catalog_generation == generation).unwrap_or(false);
            if !current {
                return;
            }
            let write = cx.background_executor().spawn(async move {
                with_catalog(&state, |c| targets.iter().try_for_each(|&id| mark.write(c, id).map(drop)))
            });
            let result = write.await;
            this.update(cx, |s, cx| {
                if s.catalog_generation != generation {
                    return;
                }
                if let Err(e) = result {
                    eprintln!("library: mark failed: {e}");
                    s.model.update(cx, |m, cx| m.set_status(format!("Could not mark: {e}"), cx));
                }
                if let Some(snapshot) = snapshot {
                    s.library.step_active_over(&snapshot, 1, false);
                }
                s.refresh_rows(cx);
                s.refresh_scope(cx);
                s.after_input(cx);
            })
            .ok();
        }));
    }

    /// Run burst-relative sharpness analysis over the selection, else the whole view
    /// (App.tsx's `runBurstAnalysis`), off the UI thread; report on the status line and
    /// re-read the rows for the new badges.
    pub fn analyse_burst(&mut self, cx: &mut Context<Self>) {
        let targets = self.whole_view_targets();
        if targets.is_empty() {
            self.model.update(cx, |m, cx| m.set_status("No photos to analyse — scan or select some first.", cx));
            return;
        }
        let status = format!("Analysing burst sharpness for {} photos…", targets.len());
        self.model.update(cx, |m, cx| m.set_status(status, cx));
        let generation = self.catalog_generation;
        let state = self.app.clone();
        let run = cx.background_executor().spawn(async move {
            chairphoto_core::burst_analysis::analyze_burst_sharpness(&state, &targets)
        });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |s, cx| {
                if s.catalog_generation != generation {
                    return;
                }
                let line = match result {
                    Ok(r) => format!(
                        "Burst analysis done — {} cluster(s), {} best frame(s), {} soft-in-burst.",
                        r.clusters, r.flagged_best, r.flagged_soft
                    ),
                    Err(e) => format!("Burst analysis failed: {e}"),
                };
                s.model.update(cx, |m, cx| m.set_status(line, cx));
                s.refresh_rows(cx);
            })
            .ok();
        })
        .detach();
    }

    /// What the whole-view tools (burst analysis, stack proposals) act on: the selection,
    /// else every row of the current view — not the active photo alone (App.tsx).
    pub fn whole_view_targets(&self) -> Vec<i64> {
        let ids = self.library.selection().ids.to_vec();
        if ids.is_empty() {
            self.library.photo_ids()
        } else {
            ids
        }
    }

    // --- deep links ------------------------------------------------------------------

    /// A resolved `chairphoto://` link (App.tsx's deep-link effects): back to the Library;
    /// a photo widens the scope to the whole library (keeping the sort) and is selected
    /// once the grid lists it; a tag becomes the scope.
    pub fn apply_deep_link(&mut self, target: DeepLinkTarget, cx: &mut Context<Self>) {
        self.surface = Surface::Library;
        match target {
            DeepLinkTarget::Photo { photo, view, .. } => {
                self.pending_link = Some(PendingPhotoLink { photo: *photo, view });
                // `clear_scope` always changes the query, so the rows are re-read and the
                // link is applied when they land.
                self.update_scope(cx, |l| l.clear_scope());
            }
            DeepLinkTarget::Tag { id, .. } => {
                self.pending_link = None;
                self.update_scope(cx, |l| l.select_tag(Some(id)));
            }
        }
        cx.notify();
    }

    /// Select a waiting link's photo once the rows hold it; a stacked child, which the
    /// grid never lists, is viewed off-grid. Otherwise keep waiting for the next rows.
    fn apply_pending_link(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.pending_link.take() else { return };
        let id = link.photo.id;
        if self.library.photos().iter().any(|p| p.id == id) {
            self.library.select(id, SelectMods::default());
        } else if link.photo.stack_parent_id.is_some() {
            self.library.view_photo(link.photo.clone());
        } else {
            self.pending_link = Some(link);
            return;
        }
        match link.view {
            DeepLinkView::Grid => {}
            DeepLinkView::Loupe => self.model.update(cx, |m, cx| m.not_yet_ported("Deep link into the loupe", 109, cx)),
            DeepLinkView::Develop => {
                self.model.update(cx, |m, cx| m.not_yet_ported("Deep link into the Darkroom", 111, cx))
            }
        }
    }

    // --- events and reads ---------------------------------------------------------------

    pub(crate) fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        let jobs_changed = self.jobs.on_core_event(event);
        match event {
            CoreEvent::CatalogSwitched(_) => {
                // Every id in the session names something in the catalog that just closed.
                let before = self.library.query_revision();
                self.library.reset();
                self.rows_loaded = false;
                self.rows_pending = None;
                self.pending_link = None;
                self.catalog_generation += 1;
                self.surface = Surface::Library;
                self.counts = Counts::default();
                self.scope_info = ScopeInfo::default();
                self.lists = Lists::default();
                if self.library.query_revision() != before {
                    cx.emit(ScopeChanged { query_revision: self.library.query_revision() });
                }
                // The superseded reads' results are dropped; the new catalog's arrive with
                // the model's `CatalogRead`.
                self.lists_generation += 1;
                self.scope_generation += 1;
                self.counts_generation += 1;
                cx.notify();
            }
            _ if jobs_changed => cx.notify(),
            _ => {}
        }
    }

    /// Re-read the lists, the counts and the scope's match count, off the UI thread.
    pub fn refresh_catalog_data(&mut self, cx: &mut Context<Self>) {
        self.lists_generation += 1;
        self.counts_generation += 1;
        let (generation, counts_generation) = (self.lists_generation, self.counts_generation);
        let state = self.app.clone();
        // GPUI's background executor, as `AppModel::refresh` (and for the same reason: the
        // deterministic test scheduler). Short catalog reads only.
        let read = cx.background_executor().spawn(async move { (read_lists(&state), read_counts(&state)) });
        cx.spawn(async move |this, cx| {
            let (lists, counts) = read.await;
            this.update(cx, |s, cx| {
                if s.lists_generation == generation {
                    match lists {
                        Ok((lists, soft_threshold)) => {
                            s.lists = lists;
                            s.soft_threshold = soft_threshold;
                        }
                        Err(e) => eprintln!("shell: lists unavailable: {e}"),
                    }
                    if counts.identity_debt.is_some() {
                        s.counts.identity_debt = counts.identity_debt;
                    }
                }
                // The back-up queue and trash counts are also written by `refresh_on_focus`; a
                // newer read of either kind supersedes this one.
                if s.counts_generation == counts_generation {
                    s.counts.pending = counts.pending;
                    s.counts.trash = counts.trash.or(s.counts.trash);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.refresh_scope(cx);
        self.refresh_rows(cx);
    }

    /// The main window regained focus: re-read the back-up queue and the trash count, as
    /// React's `onFocus` did (`checkReconcile` + `refreshTrashCount`; the reconcile it could
    /// start is Storage and import, #114). Off the UI thread; a read that a newer focus or a
    /// catalog switch superseded is dropped. No-op with no catalog open.
    pub fn refresh_on_focus(&mut self, cx: &mut Context<Self>) {
        self.counts_generation += 1;
        let generation = self.counts_generation;
        let state = self.app.clone();
        let read = cx.background_executor().spawn(async move {
            let pending = with_catalog(&state, |c| c.list_pending_operations())
                .map(|ops| ops.iter().filter(|o| o.status == "pending").count());
            let trash = with_catalog(&state, |c| c.list_trash()).map(|t| t.len());
            (pending, trash)
        });
        cx.spawn(async move |this, cx| {
            let (pending, trash) = read.await;
            this.update(cx, |s, cx| {
                if s.counts_generation != generation {
                    return;
                }
                // A failed read (e.g. no catalog yet) leaves the counts as they were: unknown
                // is not zero.
                if let Ok(pending) = pending {
                    s.counts.pending = pending;
                }
                if let Ok(trash) = trash {
                    s.counts.trash = Some(trash);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read what the current scope resolves to.
    fn refresh_scope(&mut self, cx: &mut Context<Self>) {
        self.scope_generation += 1;
        let generation = self.scope_generation;
        let state = self.app.clone();
        let scope = self.library.scope().clone();
        let read = cx.background_executor().spawn(async move {
            with_catalog(&state, |c| {
                let query: PhotoQuery = scope.query();
                let total = c.count_photos(&query)?;
                let tag_name = match scope.tag_id {
                    Some(id) => c.get_tag(id).ok().map(|t| t.name),
                    None => None,
                };
                let album_name = match scope.album_id {
                    Some(id) => c.list_albums()?.into_iter().find(|a| a.id == id).map(|a| a.name),
                    None => None,
                };
                let smart_album_name = match scope.smart_album_id {
                    Some(id) => c.list_smart_albums()?.into_iter().find(|a| a.id == id).map(|a| a.name),
                    None => None,
                };
                Ok(ScopeInfo { total: Some(total), tag_name, album_name, smart_album_name })
            })
        });
        cx.spawn(async move |this, cx| {
            let info = read.await;
            this.update(cx, |s, cx| {
                if s.scope_generation != generation {
                    return;
                }
                match info {
                    Ok(info) => s.scope_info = info,
                    Err(e) => {
                        // Keep the names; drop a count that no longer describes the scope.
                        s.scope_info.total = None;
                        eprintln!("shell: scope count unavailable: {e}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// The lists the menus offer, and the soft-badge threshold. Blocking: background only.
fn read_lists(state: &AppState) -> Result<(Lists, f64), String> {
    with_catalog(state, |c| {
        let soft_threshold = c
            .get_setting(SOFT_THRESHOLD_KEY)?
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(SOFT_THRESHOLD_DEFAULT);
        let lists = Lists {
            facets: c.available_facets(),
            cameras: c.distinct_photo_values("camera")?,
            lenses: c.distinct_photo_values("lens")?,
            batches: c.list_import_batches()?,
            albums: c.list_albums()?,
            smart_albums: c.list_smart_albums()?,
        };
        Ok((lists, soft_threshold))
    })
}

/// The attention counts, each read on its own so one failure leaves the others. A failed
/// identity-debt or trash read is `None` ("unknown"); a failed pending read counts 0, as
/// React's `refreshPending` did.
fn read_counts(state: &AppState) -> Counts {
    let pending = with_catalog(state, |c| c.list_pending_operations())
        .map(|ops| ops.iter().filter(|o| o.status == "pending").count())
        .unwrap_or(0);
    let identity_debt = with_catalog(state, |c| c.summarize_pending_identity()).ok().map(|s| s.total);
    let trash = with_catalog(state, |c| c.list_trash()).ok().map(|t| t.len());
    Counts { pending, identity_debt, trash }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chairphoto_core::app::ImportProgress;
    use chairphoto_core::scanner::ScanProgress;

    #[test]
    fn column_widths_clamp_to_reacts_drag_limits() {
        assert_eq!(clamp_column(10.), 140.);
        assert_eq!(clamp_column(300.), 300.);
        assert_eq!(clamp_column(9000.), 640.);
    }

    #[test]
    fn thumb_sizes_snap_to_the_sliders_steps() {
        assert_eq!(snap_thumb(160.), 160.);
        assert_eq!(snap_thumb(163.), 160.);
        assert_eq!(snap_thumb(165.), 168.);
        assert_eq!(snap_thumb(50.), 120.);
        assert_eq!(snap_thumb(400.), 320.);
    }

    fn scan(phase: &str, done: usize, total: usize) -> CoreEvent {
        CoreEvent::ScanProgress(ScanProgress { phase: phase.into(), done, total })
    }

    #[test]
    fn bench_progress_prefers_import_then_scan_then_develop_with_reacts_labels() {
        let mut jobs = Jobs::default();
        assert_eq!(jobs.bench_progress(), None);
        jobs.on_core_event(&scan("indexing", 1500, 0));
        assert_eq!(
            jobs.bench_progress(),
            Some(Progress { label: "Indexing 1,500…".into(), done: 1500, total: None })
        );
        jobs.on_core_event(&scan("metadata", 10, 2000));
        assert_eq!(jobs.bench_progress().unwrap().label, "Reading metadata 10/2,000");
        jobs.import_job = Some(4);
        jobs.on_core_event(&CoreEvent::ImportProgress(ImportProgress { job: 4, done: 3, total: 9 }));
        assert_eq!(
            jobs.bench_progress(),
            Some(Progress { label: "Importing 3/9".into(), done: 3, total: Some(9) })
        );
        jobs.import = None;
        jobs.on_core_event(&scan("finalizing", 0, 0));
        assert_eq!(jobs.bench_progress().unwrap().label, "Finalizing…");
        jobs.on_core_event(&scan("done", 0, 0));
        assert_eq!(jobs.bench_progress(), None);
    }

    #[test]
    fn a_catalog_switch_clears_every_job() {
        let mut jobs = Jobs::default();
        jobs.import_job = Some(1);
        jobs.on_core_event(&CoreEvent::ImportProgress(ImportProgress { job: 1, done: 0, total: 0 }));
        assert_eq!(jobs.bench_progress().unwrap().label, "Importing …");
        assert!(jobs.on_core_event(&CoreEvent::CatalogSwitched("x".into())));
        assert_eq!(jobs, Jobs::default());
    }
}
