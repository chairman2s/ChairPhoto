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

use crate::model::{AppModel, AppModelEvent};
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use chairphoto_core::catalog::{Album, Facet, ImportBatch, PhotoQuery, SmartAlbum};
use chairphoto_model::library::session::LibrarySession;
use gpui_kit::{Context, Entity, EventEmitter, Subscription};

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
    /// A module's main view (Module registry, #104), by view id.
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
    /// `import:progress` — cleared by the import flow when it ends (Storage and import,
    /// #114) and on a catalog switch.
    pub import: Option<(usize, usize)>,
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
            CoreEvent::ImportProgress(p) => self.import = Some((p.done, p.total)),
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

    // --- the Library scope and selection ------------------------------------------------

    /// Run one of the session's scope verbs; when the derived query changed, re-read the
    /// match count and chip names, and tell the Library view.
    pub fn update_scope(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut LibrarySession)) {
        let before = self.library.query_revision();
        f(&mut self.library);
        let after = self.library.query_revision();
        if after != before {
            self.refresh_scope(cx);
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

    // --- events and reads ---------------------------------------------------------------

    pub(crate) fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        let jobs_changed = self.jobs.on_core_event(event);
        match event {
            CoreEvent::CatalogSwitched(_) => {
                // Every id in the session names something in the catalog that just closed.
                let before = self.library.query_revision();
                self.library.reset();
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
                        Ok(lists) => s.lists = lists,
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

/// The lists the menus offer. Blocking: background only.
fn read_lists(state: &AppState) -> Result<Lists, String> {
    with_catalog(state, |c| {
        Ok(Lists {
            facets: c.available_facets(),
            cameras: c.distinct_photo_values("camera")?,
            lenses: c.distinct_photo_values("lens")?,
            batches: c.list_import_batches()?,
            albums: c.list_albums()?,
            smart_albums: c.list_smart_albums()?,
        })
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
        jobs.on_core_event(&CoreEvent::ImportProgress(ImportProgress { done: 3, total: 9 }));
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
        jobs.on_core_event(&CoreEvent::ImportProgress(ImportProgress { done: 0, total: 0 }));
        assert_eq!(jobs.bench_progress().unwrap().label, "Importing …");
        assert!(jobs.on_core_event(&CoreEvent::CatalogSwitched("x".into())));
        assert_eq!(jobs, Jobs::default());
    }
}
