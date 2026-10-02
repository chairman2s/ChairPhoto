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
//! `panel.inspectorTab`, `panel.section.*`). The per-machine store for them exists now
//! ([`crate::machine_prefs::MachinePrefs`], #113, which holds the appearance mode), but these
//! keys are not written to it yet, so they start at React's defaults each launch.

use crate::loupe::card::{LoupeCard, ShownCard};
use crate::loupe::compare::{CompareMode, CompareSession, Verdict};
use crate::model::{AppModel, AppModelEvent, DeepLinkTarget};
use chairphoto_core::app::{
    with_catalog, with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent, ExportKind,
};
use chairphoto_core::catalog::{
    Album, Catalog, Facet, ImportBatch, Photo, PhotoPage, PhotoQuery, PhotoVersion, PickState, SmartAlbum,
    SOFT_THRESHOLD_DEFAULT, SOFT_THRESHOLD_KEY,
};
use chairphoto_model::deep_link::DeepLinkView;
use chairphoto_model::library::query::{RefreshRequest, StatusRequest};
use chairphoto_model::compare_duel::DuelSide;
use chairphoto_model::library::session::{LibrarySession, SelectMods, StepSnapshot};
use futures::channel::oneshot;
use gpui_kit::{Context, Entity, EventEmitter, SharedString, Subscription, Task};

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
    /// The Darkroom (#111, `crate::darkroom`): it develops the active photo.
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
    /// The photo export the bench follows (`export:progress` of kind `photos` and this job),
    /// set and cleared by `crate::export::ExportState`; a straggler of any other job moves
    /// nothing.
    pub export_photos: Option<ExportTrack>,
    /// The bundle export the bench follows, likewise.
    pub export_bundle: Option<ExportTrack>,
}

/// An export the bench follows: its job id and its last `(done, total)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportTrack {
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

impl ExportTrack {
    pub fn new(job: u64) -> Self {
        ExportTrack { job, done: 0, total: 0 }
    }
}

impl Jobs {
    /// The one job worth showing: import, then scan, then develop — React's labels.
    pub fn bench_progress(&self) -> Option<Progress> {
        if let Some((done, total)) = self.import {
            let label = if total > 0 { format!("Importing {done}/{total}") } else { "Importing …".into() };
            return Some(Progress { label, done, total: (total > 0).then_some(total) });
        }
        for (track, verb) in [(&self.export_photos, "Exporting"), (&self.export_bundle, "Writing bundle")] {
            if let Some(t) = track {
                let label = if t.total > 0 { format!("{verb} {}/{}", t.done, t.total) } else { format!("{verb} …") };
                return Some(Progress { label, done: t.done, total: (t.total > 0).then_some(t.total) });
            }
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
            CoreEvent::ExportProgress(p) => {
                let track = match p.kind {
                    ExportKind::Photos => &mut self.export_photos,
                    ExportKind::Bundle => &mut self.export_bundle,
                };
                match track {
                    Some(t) if t.job == p.job => (t.done, t.total) = (p.done, p.total),
                    _ => return false,
                }
            }
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

/// What a queued culling write does once it has run.
enum AfterMark {
    /// Re-read the rows and the scope, and — for the keyboard — step past the photo marked
    /// (over the rows as they were when the key was pressed). Failures go to the status line.
    Refresh(Option<StepSnapshot>),
    /// Report the result to the caller and touch nothing else: the cull session records its
    /// own decisions and re-reads the rows once, at its end (CullSession.tsx).
    Report(oneshot::Sender<Result<(), String>>),
    /// A Compare verdict's writes: as `Refresh(None)`, and settle the verdict — written, the
    /// duel moves on; failed, it stays on the same pair (React awaited `applyMark`).
    Verdict(Verdict),
}

/// What the Library stage shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageView {
    Grid,
    /// The inline loupe on the active photo.
    Loupe,
    Compare,
}

/// The Darkroom's working state as a print on the pop-out loupe (DarkroomView.tsx's "🖥 Loupe
/// print", `broadcastPrint`): the photo, the stamped record and the pixels it renders from, so
/// the pop-out shows the stage's own render. Set and cleared by the Darkroom.
#[cfg(feature = "edit")]
#[derive(Debug, Clone)]
pub struct LoupePrint {
    pub photo: Photo,
    pub edit_json: String,
    pub source: chairphoto_core::plugins::edit::SourceToken,
}

/// A `chairphoto://<uuid>` link waiting for the widened grid to list its photo (App.tsx's
/// `deepLinkTarget`).
#[derive(Debug, Clone)]
pub struct PendingPhotoLink {
    pub photo: Photo,
    pub view: DeepLinkView,
    /// The catalog the link resolved against: it is applied only over rows from that one.
    pub from: CatalogIdentity,
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
    /// The catalog the rows — and so every id in the selection — were read from. Every
    /// write keyed by those ids (marks, burst analysis, stacks, the inspector's) is bound to
    /// it (`with_catalog_as`), so it fails closed once another catalog is open, even before
    /// `catalog:switched` reaches the shell. `None` until rows land, and after a switch.
    rows_from: Option<CatalogIdentity>,
    /// The catalog [`Self::lists`] were read from: what a write keyed by their ids (an album,
    /// a smart album, an import batch) is bound to. `None` until they land, and after a switch.
    lists_from: Option<CatalogIdentity>,
    /// A photo link waiting for the grid to list its photo.
    pub pending_link: Option<PendingPhotoLink>,
    /// The version of the active photo the loupe shows (`None` = Original): picked in the
    /// inspector's Versions tab (App.tsx's `activeVersion`). It belongs to one photo, and is
    /// dropped when the active photo changes or the catalog switches.
    active_version: Option<PhotoVersion>,
    /// The tag the tag editor is showing, if one is open: what `tag-editor`-slot module
    /// panels edit (host.ts's `getEditingTag`). Set and cleared by `tags::editor::TagEditor`.
    pub editing_tag: Option<i64>,
    /// The catalog the editing tag's id was read from (the tree the editor opened over), so a
    /// panel can bind its reads and writes to it. `None` while no tag is edited.
    pub editing_tag_from: Option<CatalogIdentity>,
    /// The inline loupe is on (App.tsx's `loupeInline`): the stage shows the active photo
    /// instead of the grid while there is one. Kept across a cleared selection, as React did.
    pub loupe_open: bool,
    /// Compare, while open, and the catalog its pool was read from (#109).
    compare: Option<(CompareSession, CatalogIdentity)>,
    /// The card a module put up in the pop-out loupe, with its owner (#110,
    /// `crate::loupe::card`).
    loupe_card: Option<ShownCard>,
    /// The Darkroom's print on the pop-out loupe, while one is up (#110).
    #[cfg(feature = "edit")]
    loupe_print: Option<LoupePrint>,
    /// Compare's presentation for the next open (`panel.compareMode`; the root view seeds it
    /// from the per-machine preferences and stores changes back).
    pub compare_mode: CompareMode,
    /// The newest culling write: each write waits for the one before, so marks land in the
    /// order they were made.
    last_mark: Option<Task<()>>,
    /// Bumped by every `catalog:switched`: a mark or burst analysis queued before it names
    /// photos of the catalog that closed and must not be written.
    catalog_generation: u64,
    /// The newest burst analysis's job id (`burst_analysis::next_burst_job`): only its
    /// result reaches the status line and re-reads the rows ([`Self::finish_burst`]).
    burst_job: u64,
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
            rows_from: None,
            lists_from: None,
            pending_link: None,
            active_version: None,
            editing_tag: None,
            editing_tag_from: None,
            loupe_open: false,
            compare: None,
            loupe_card: None,
            #[cfg(feature = "edit")]
            loupe_print: None,
            compare_mode: CompareMode::Duel,
            last_mark: None,
            catalog_generation: 0,
            burst_job: 0,
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

    /// The tag editor opened on `tag` (`Some`), read from catalog `from`, or closed (`None`).
    pub fn set_editing_tag(&mut self, tag: Option<i64>, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        let from = tag.and(from);
        if self.editing_tag != tag || self.editing_tag_from != from {
            self.editing_tag = tag;
            self.editing_tag_from = from;
            cx.notify();
        }
    }

    pub fn toggle_cache_previews(&mut self, cx: &mut Context<Self>) {
        self.cache_previews = !self.cache_previews;
        cx.notify();
    }

    pub fn show_library(&mut self, cx: &mut Context<Self>) {
        self.surface = Surface::Library;
        cx.notify();
    }

    /// The rail's Develop, an inspector version's ✎, a `develop` link: the Darkroom develops
    /// the active photo (`crate::darkroom::Darkroom` follows the surface). Without an active
    /// photo there is nothing to develop: the status line says so.
    pub fn open_develop(&mut self, cx: &mut Context<Self>) {
        if self.library.selection().active.is_none() {
            self.model.update(cx, |m, cx| m.set_status("Select a photo to develop.", cx));
            return;
        }
        self.surface = Surface::Develop;
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
        self.drop_foreign_version();
        cx.notify();
    }

    /// The active photo's chosen version, if one is chosen (`None` = Original).
    pub fn active_version(&self) -> Option<&PhotoVersion> {
        let active = self.library.selection().active_id;
        self.active_version.as_ref().filter(|v| Some(v.photo_id) == active)
    }

    /// Choose the version the loupe shows; one of another photo than the active one is
    /// refused (it would draw that photo's edit on this one).
    pub fn set_active_version(&mut self, version: Option<PhotoVersion>, cx: &mut Context<Self>) {
        let active = self.library.selection().active_id;
        self.active_version = version.filter(|v| Some(v.photo_id) == active);
        cx.notify();
    }

    /// App.tsx reset the active version to Original whenever the active photo changed.
    fn drop_foreign_version(&mut self) {
        let active = self.library.selection().active_id;
        if self.active_version.as_ref().is_some_and(|v| Some(v.photo_id) != active) {
            self.active_version = None;
        }
    }

    /// Run a selection verb (a click, a key), then ask for the active photo's storage badge
    /// if it changed — what React's effect on the active id did after a commit.
    pub fn select_with(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut LibrarySession)) {
        f(&mut self.library);
        self.after_input(cx);
    }

    fn after_input(&mut self, cx: &mut Context<Self>) {
        self.drop_foreign_version();
        if let Some(request) = self.library.take_active_status_request() {
            self.fetch_statuses(request, cx);
        }
        cx.notify();
    }

    // --- loupe and Compare (#109) ---------------------------------------------------------

    /// What the stage shows: Compare while it has a pane to show, else the loupe while it is
    /// on and a photo is active, else the grid.
    pub fn stage_view(&self) -> StageView {
        if !self.compare_panes().is_empty() {
            StageView::Compare
        } else if self.loupe_open && self.library.selection().active_id.is_some() {
            StageView::Loupe
        } else {
            StageView::Grid
        }
    }

    /// Enter (with an active photo) or the More menu's "Loupe": toggle the inline loupe.
    pub fn toggle_loupe(&mut self, cx: &mut Context<Self>) {
        let open = !self.loupe_open && self.library.selection().active_id.is_some();
        self.set_loupe(open, cx);
    }

    pub fn set_loupe(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.loupe_open != open {
            self.loupe_open = open;
            cx.notify();
        }
    }

    /// Open `photo` in the loupe even when the grid does not list it — a stacked child
    /// (App.tsx's `viewPhotoInLoupe`: the session holds it aside, the shell opens the loupe).
    pub fn view_in_loupe(&mut self, photo: Photo, cx: &mut Context<Self>) {
        self.compare = None;
        self.loupe_open = true;
        self.select_with(cx, |l| l.view_photo(photo));
    }

    /// The photo the loupe and the inspector follow (`shellTarget.ts`): Compare's focused
    /// pane while Compare is open, else the active photo.
    pub fn loupe_target(&self) -> Option<&Photo> {
        if let Some((session, _)) = &self.compare {
            let panes = self.compare_panes();
            let ids: Vec<i64> = panes.iter().map(|p| p.id).collect();
            if let Some(id) = session.focused(&ids) {
                return panes.into_iter().find(|p| p.id == id);
            }
        }
        self.library.selection().active
    }

    /// The card a module has up in the pop-out loupe, if any.
    pub fn loupe_card(&self) -> Option<&ShownCard> {
        self.loupe_card.as_ref()
    }

    /// Module `module` puts `card` up in the pop-out loupe (host.ts `showLoupeCard`), bound to
    /// the catalog `from`; `None` takes down its own card and leaves another module's alone.
    pub fn show_loupe_card(
        &mut self,
        module: SharedString,
        card: Option<LoupeCard>,
        from: Option<CatalogIdentity>,
        cx: &mut Context<Self>,
    ) {
        match card {
            Some(card) => {
                let shown = ShownCard { module, card, from };
                if self.loupe_card.as_ref() == Some(&shown) {
                    return;
                }
                self.loupe_card = Some(shown);
            }
            None if self.loupe_card.as_ref().is_some_and(|c| c.module == module) => self.loupe_card = None,
            None => return,
        }
        cx.notify();
    }

    /// The Darkroom's print on the pop-out loupe, if one is up.
    #[cfg(feature = "edit")]
    pub fn loupe_print(&self) -> Option<&LoupePrint> {
        self.loupe_print.as_ref()
    }

    /// Put the Darkroom's print up on the pop-out loupe (`None`: the pop-out follows the
    /// target again). The Darkroom's "🖥 Loupe print" (`Darkroom::set_print_on_loupe`).
    #[cfg(feature = "edit")]
    pub fn set_loupe_print(&mut self, print: Option<LoupePrint>, cx: &mut Context<Self>) {
        self.loupe_print = print;
        cx.notify();
    }

    /// Open Compare on the selection (two or more; C in the grid, the bench's Compare). The
    /// pool is frozen with the catalog its ids were read from. Closes the loupe.
    pub fn open_compare(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(from) = self.rows_from else { return false };
        let selection = self.library.selection();
        let Some(session) = CompareSession::open(selection.ids, selection.active_id, self.compare_mode) else {
            return false;
        };
        self.compare = Some((session, from));
        self.loupe_open = false;
        cx.notify();
        true
    }

    pub fn close_compare(&mut self, cx: &mut Context<Self>) {
        if self.compare.take().is_some() {
            cx.notify();
        }
    }

    pub fn compare(&self) -> Option<&CompareSession> {
        self.compare.as_ref().map(|(s, _)| s)
    }

    /// The panes on screen: this round's ids, looked up in the current rows (a rating shows
    /// on its own pane); a row that vanished is left out.
    pub fn compare_panes(&self) -> Vec<&Photo> {
        let Some((session, _)) = &self.compare else { return Vec::new() };
        let photos = self.library.photos();
        session.batch().into_iter().filter_map(|id| photos.iter().find(|p| p.id == id)).collect()
    }

    fn compare_pane_ids(&self) -> Vec<i64> {
        self.compare_panes().iter().map(|p| p.id).collect()
    }

    /// Change Compare's state with the ids of the panes on screen; the shell re-renders.
    pub fn update_compare(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut CompareSession, &[i64])) {
        let ids = self.compare_pane_ids();
        if let Some((session, _)) = &mut self.compare {
            f(session, &ids);
            cx.notify();
        }
    }

    /// Duel/Grid: switch the open Compare and remember the mode for the next one.
    pub fn set_compare_mode(&mut self, mode: CompareMode, cx: &mut Context<Self>) {
        self.compare_mode = mode;
        if let Some((session, _)) = &mut self.compare {
            session.switch_mode(mode);
        }
        cx.notify();
    }

    /// The focused pane, which the culling keys and the bench's marks act on in Compare.
    pub fn compare_focused(&self) -> Option<i64> {
        let (session, _) = self.compare.as_ref()?;
        session.focused(&self.compare_pane_ids())
    }

    /// ←/→ in a duel: the verdict for that side (loser rejected, last winner picked).
    pub fn compare_verdict(&mut self, side: DuelSide, cx: &mut Context<Self>) {
        let Some((session, from)) = &mut self.compare else { return };
        let from = *from;
        if let Some(verdict) = session.verdict(side) {
            self.write_verdict(verdict, from, cx);
        }
    }

    /// K / "Keep this" / "This one wins" on `keeper` (`None`: the focused pane).
    pub fn compare_keep(&mut self, keeper: Option<i64>, cx: &mut Context<Self>) {
        let Some(keeper) = keeper.or_else(|| self.compare_focused()) else { return };
        let Some((session, from)) = &mut self.compare else { return };
        let from = *from;
        if let Some(verdict) = session.keep(keeper) {
            self.write_verdict(verdict, from, cx);
        }
    }

    /// Queue the verdict's writes as one job, in order, stopping at the first that fails;
    /// the session moves on once they are all in ([`CompareSession::settle`]).
    fn write_verdict(&mut self, verdict: Verdict, from: CatalogIdentity, cx: &mut Context<Self>) {
        let writes = verdict.writes.iter().map(|&(id, pick)| (Mark::Pick(pick), id)).collect();
        self.queue_writes(writes, from, AfterMark::Verdict(verdict), cx);
        cx.notify();
    }

    // --- the Library's rows ----------------------------------------------------------

    /// The catalog the rows shown were read from ([`Self::rows_from`]'s field docs): what a
    /// write keyed by their ids, or by the selection's, is bound to.
    pub fn rows_from(&self) -> Option<CatalogIdentity> {
        self.rows_from
    }

    /// The catalog the lists shown were read from ([`Self::lists_from`]'s field docs).
    pub fn lists_from(&self) -> Option<CatalogIdentity> {
        self.lists_from
    }

    /// Tests: the generation of the row read in flight, if any.
    #[cfg(all(test, feature = "edit"))]
    pub(crate) fn rows_pending(&self) -> Option<u64> {
        self.rows_pending
    }

    /// Re-run the current query off the UI thread (`list_photos`), with the identity of the
    /// catalog it read. Only the newest read lands: the session drops a page whose
    /// generation is stale.
    pub fn refresh_rows(&mut self, cx: &mut Context<Self>) {
        let request = self.library.refresh();
        self.rows_pending = Some(request.generation);
        let state = self.app.clone();
        let query = request.query.clone();
        let read =
            cx.background_executor().spawn(async move { with_catalog_identified(&state, |c| c.photo_page(&query)) });
        cx.spawn(async move |this, cx| {
            let page = read.await;
            this.update(cx, |s, cx| s.on_page(&request, page, cx)).ok();
        })
        .detach();
    }

    /// A row read's answer. A page from another catalog than the rows shown (the core has
    /// switched, `catalog:switched` is still on its way; or a re-root reopened the catalog)
    /// empties the selection first: its ids were chosen from the other catalog's rows.
    pub(crate) fn on_page(
        &mut self,
        request: &RefreshRequest,
        page: Result<(CatalogIdentity, PhotoPage), String>,
        cx: &mut Context<Self>,
    ) {
        let landed = self.rows_pending == Some(request.generation);
        let page = match page {
            Ok((from, page)) => {
                if landed {
                    if self.rows_from.is_some_and(|shown| shown != from) {
                        self.library.clear_selection();
                        self.pending_link = None;
                        self.active_version = None;
                        // Compare's pool held the other catalog's ids.
                        self.compare = None;
                    }
                    self.rows_from = Some(from);
                }
                Ok(page)
            }
            Err(e) => Err(e),
        };
        match self.library.apply_page(request, page) {
            Ok(statuses) => {
                if let Some(statuses) = statuses {
                    self.fetch_statuses(statuses, cx);
                }
                if landed {
                    self.rows_pending = None;
                    self.rows_loaded = true;
                    // Every pane dropped out of the view (a filter the culling just failed,
                    // say): Compare ends rather than linger off screen swallowing the marks.
                    // React's `inCompare` required a pane; the grid's keys then marked the
                    // selection again.
                    if self.compare.is_some() && self.compare_panes().is_empty() {
                        self.compare = None;
                    }
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
    /// `advance` is the keyboard's own behaviour: when exactly one photo was marked — and the
    /// write succeeded — step to the next one — over the rows as they were when the key was pressed, not the
    /// refreshed ones the photo may have been filtered out of (`step_active_over`). The
    /// bench's controls pass `false`: clicking a star must not move the selection.
    ///
    /// Writes run off the UI thread, one after another in the order they were made, each
    /// bound to the catalog its ids were read from ([`Self::rows_from`]): once another
    /// catalog is open the write fails closed (`CATALOG_CHANGED`) and the status line says
    /// so — whether or not `catalog:switched` has reached the shell yet. A write still
    /// queued when the event arrives is dropped without running.
    ///
    /// While Compare is open the mark goes to its focused pane alone, never advancing: Compare
    /// exists to separate one frame from its neighbours, which rating the whole selection
    /// would defeat (App.tsx's Compare key branch and `applyMark`).
    pub fn apply_mark(&mut self, mark: Mark, advance: bool, cx: &mut Context<Self>) {
        if let Some((_, from)) = &self.compare {
            let from = *from;
            if let Some(focused) = self.compare_focused() {
                self.queue_mark(mark, vec![focused], from, AfterMark::Refresh(None), cx);
            }
            return;
        }
        let targets = self.library.selection().targets;
        let Some(from) = self.rows_from else { return };
        if targets.is_empty() {
            return;
        }
        let snapshot = (advance && targets.len() == 1).then(|| self.library.step_snapshot());
        self.queue_mark(mark, targets, from, AfterMark::Refresh(snapshot), cx);
    }

    /// [`apply_mark`](Self::apply_mark) on named photos rather than the selection, never
    /// advancing: the inspector's stars, pick and label controls mark the photo it shows
    /// (`PhotoInspector.tsx` wrote `photo.id` only, not the selection). `from` is the catalog
    /// the caller read `targets` from. The same queue, so these marks and the keys' land in
    /// the order they were made.
    pub fn apply_mark_to(&mut self, mark: Mark, targets: Vec<i64>, from: CatalogIdentity, cx: &mut Context<Self>) {
        if !targets.is_empty() {
            self.queue_mark(mark, targets, from, AfterMark::Refresh(None), cx);
        }
    }

    /// One cull-session decision on `photo` (read from `from`): the same queue as every other
    /// mark, so it lands in order with them, but without the per-write row re-read — the
    /// session re-reads once when it ends. The answer says whether the catalog took it; a
    /// write dropped by a catalog switch answers [`chairphoto_core::app::CATALOG_CHANGED`].
    pub fn apply_mark_reported(
        &mut self,
        mark: Mark,
        photo: i64,
        from: CatalogIdentity,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.queue_mark(mark, vec![photo], from, AfterMark::Report(tx), cx);
        rx
    }

    fn queue_mark(
        &mut self,
        mark: Mark,
        targets: Vec<i64>,
        from: CatalogIdentity,
        after: AfterMark,
        cx: &mut Context<Self>,
    ) {
        let writes = targets.into_iter().map(|id| (mark.clone(), id)).collect();
        self.queue_writes(writes, from, after, cx);
    }

    /// [`queue_mark`](Self::queue_mark) with a mark per photo: written in order, stopping at
    /// the first failure.
    fn queue_writes(
        &mut self,
        writes: Vec<(Mark, i64)>,
        from: CatalogIdentity,
        after: AfterMark,
        cx: &mut Context<Self>,
    ) {
        let previous = self.last_mark.take();
        let generation = self.catalog_generation;
        let state = self.app.clone();
        self.last_mark = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let current = this.update(cx, |s, _| s.catalog_generation == generation).unwrap_or(false);
            if !current {
                if let AfterMark::Report(tx) = after {
                    let _ = tx.send(Err(chairphoto_core::app::CATALOG_CHANGED.into()));
                }
                return;
            }
            let write = cx.background_executor().spawn(async move {
                with_catalog_as(&state, from, |c| writes.iter().try_for_each(|(mark, id)| mark.write(c, *id).map(drop)))
            });
            let result = write.await;
            let (snapshot, verdict) = match after {
                AfterMark::Report(tx) => {
                    let _ = tx.send(result);
                    return;
                }
                AfterMark::Refresh(snapshot) => (snapshot, None),
                AfterMark::Verdict(verdict) => (None, Some(verdict)),
            };
            this.update(cx, |s, cx| {
                if s.catalog_generation != generation {
                    return;
                }
                if let (Some(verdict), Some((session, _))) = (&verdict, &mut s.compare) {
                    session.settle(verdict, result.is_ok());
                }
                match result {
                    // The keyboard advances only past a mark that was written (React advanced
                    // after `applyToSelection` resolved; a failed write threw first).
                    Ok(()) => {
                        if let Some(snapshot) = snapshot {
                            s.library.step_active_over(&snapshot, 1, false);
                        }
                    }
                    Err(e) => {
                        eprintln!("library: mark failed: {e}");
                        s.model.update(cx, |m, cx| m.set_status(format!("Could not mark: {e}"), cx));
                    }
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
    /// re-read the rows for the new badges. Bound to the catalog the rows came from
    /// (`analyze_burst_sharpness_as`): after a switch it fails closed rather than flag the
    /// new catalog's photos that carry the same ids. A newer run makes this one unreachable:
    /// the core does not persist a superseded run's flags (`BURST_SUPERSEDED`), and the shell
    /// shows only the newest run's result ([`Self::finish_burst`]).
    pub fn analyse_burst(&mut self, cx: &mut Context<Self>) {
        let targets = self.whole_view_targets();
        let Some(from) = self.rows_from.filter(|_| !targets.is_empty()) else {
            self.model.update(cx, |m, cx| m.set_status("No photos to analyse — scan or select some first.", cx));
            return;
        };
        let status = format!("Analysing burst sharpness for {} photos…", targets.len());
        self.model.update(cx, |m, cx| m.set_status(status, cx));
        let generation = self.catalog_generation;
        let state = self.app.clone();
        // The id is allocated here, where the user started the run, so the newest start owns
        // the burst generation whichever worker claims it first (`install_fresh_if_newer`).
        let job = chairphoto_core::burst_analysis::next_burst_job(&state);
        self.burst_job = job;
        let run = cx.background_executor().spawn(async move {
            chairphoto_core::burst_analysis::analyze_burst_sharpness_as(&state, from, &targets, job)
        });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |s, cx| s.finish_burst(job, generation, result, cx)).ok();
        })
        .detach();
    }

    /// A burst run's terminal result, shown only while `job` is still the newest run and
    /// the catalog it started on is still the shell's: a superseded run's result — its
    /// `BURST_SUPERSEDED` refusal, or a success that raced the newer start — is dropped
    /// without touching the status line or the rows.
    pub(crate) fn finish_burst(
        &mut self,
        job: u64,
        generation: u64,
        result: Result<chairphoto_core::burst_analysis::BurstAnalysisResult, String>,
        cx: &mut Context<Self>,
    ) {
        if self.catalog_generation != generation || self.burst_job != job {
            return;
        }
        let line = match result {
            Ok(r) => format!(
                "Burst analysis done — {} cluster(s), {} best frame(s), {} soft-in-burst.",
                r.clusters, r.flagged_best, r.flagged_soft
            ),
            Err(e) => format!("Burst analysis failed: {e}"),
        };
        self.model.update(cx, |m, cx| m.set_status(line, cx));
        self.refresh_rows(cx);
    }

    /// The newest burst run's job id and the catalog generation, as [`Self::finish_burst`]
    /// compares them — for tests that deliver a superseded run's late result.
    #[cfg(test)]
    pub(crate) fn burst_owner(&self) -> (u64, u64) {
        (self.burst_job, self.catalog_generation)
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
            DeepLinkTarget::Photo { photo, view, from, .. } => {
                self.pending_link = Some(PendingPhotoLink { photo: *photo, view, from });
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
        // Resolved against another catalog than these rows: its id names another photo here.
        if self.rows_from != Some(link.from) {
            return;
        }
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
            DeepLinkView::Loupe => {
                self.compare = None;
                self.loupe_open = true;
            }
            DeepLinkView::Develop => self.open_develop(cx),
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
                self.rows_from = None;
                self.lists_from = None;
                self.pending_link = None;
                self.active_version = None;
                self.editing_tag = None;
                self.editing_tag_from = None;
                self.loupe_open = false;
                self.compare = None;
                // Its photo scope names the closed catalog's tags.
                self.loupe_card = None;
                #[cfg(feature = "edit")]
                {
                    self.loupe_print = None;
                }
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
                        Ok((from, (lists, soft_threshold))) => {
                            s.lists = lists;
                            s.lists_from = Some(from);
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
fn read_lists(state: &AppState) -> Result<(CatalogIdentity, (Lists, f64)), String> {
    with_catalog_identified(state, |c| {
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
    // Copies owing identity plus photos owing IPTC (#148): both are paid by the repair pass
    // the chip opens, and a count of 0 would hide the only lasting signal of either.
    let identity_debt = with_catalog(state, |c| c.summarize_pending_identity()).ok().map(|s| s.total + s.iptc_owed);
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
