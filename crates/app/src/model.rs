//! [`AppModel`]: the app-wide state the shell shows — which catalog is open, how many photos
//! it holds, and the last thing the core reported — and where the GPUI app's data flow is
//! decided.
//!
//! # Data flow: entities, refreshed by events and by the code that mutated
//!
//! The React app reads through `src/modules/api.ts`, which keeps a **list cache**
//! (`list_tags`, `list_facets`, `distinct_photo_values`, `list_tag_groups`,
//! `recently_used_tags`, keyed by command and arguments, in-flight requests shared) and treats
//! every command not on its read-only allowlist as a mutation that drops the whole cache.
//! That cache exists because many components fetch the same list over IPC independently.
//!
//! The GPUI app has **no separate list cache**. Each list lives in exactly one entity
//! (`AppModel` today; a tag-list entity, an album-list entity … as views are ported), and every
//! view that shows it reads that entity. The entity *is* the cache: one fetch per
//! invalidation however many views read it, and `cx.notify()` redraws all of them.
//! An entity refetches — off the UI thread, dropping a result that a newer refresh has
//! superseded ([`AppModel::refresh`]'s generation) — when:
//!
//! 1. **a [`CoreEvent`] says its data changed**, routed to it by `events::route`
//!    (`catalog:switched` invalidates everything catalog-derived; a scan's terminal
//!    `scan:progress {phase: "done"}` invalidates counts and lists), or
//! 2. **the code that performed a mutation invalidates it** after the core call returns. A
//!    mutation names the entities it affects.
//!
//! api.ts's conservative rule carries over: **a mutation whose effects are unknown counts as
//! having changed everything catalog-derived** — the caller invalidates every catalog-derived
//! entity rather than guess. A stale list is a correctness bug; an extra refetch is only work.

use chairphoto_core::app::{with_catalog, with_catalog_identified, AppState, CatalogIdentity, CoreEvent, EventVisitor};
use crate::image_store::Loaded;
use chairphoto_core::catalog::{CatalogError, Photo, PhotoQuery};
use chairphoto_core::image_pool::ImagePool;
use chairphoto_model::deep_link::{self, DeepLink, DeepLinkView};
use gpui_kit::{Context, EventEmitter, SharedString};
use std::sync::Arc;

/// What the [`AppModel`] tells the entities that derive state from it (the shell today).
#[derive(Clone)]
pub enum AppModelEvent {
    /// A core event, after the model has noted it — so a subscriber applies the same
    /// invalidation rules without a second route in `events.rs`.
    Core(CoreEvent),
    /// A refresh read the open catalog: catalog-derived lists are worth (re)reading now.
    /// Startup's `open_default_catalog` sends no `catalog:switched`, so this is how the
    /// first catalog reaches the other entities.
    CatalogRead,
    /// A `chairphoto://` link resolved against the open catalog: the shell applies it to
    /// the Library (`ShellState::apply_deep_link`).
    DeepLink(DeepLinkTarget),
}

/// What the shell shows about the open catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSummary {
    /// The catalog file's name, as the React title bar shows it (`default.chairphoto`).
    pub name: String,
    /// Every photo in the library view with no filter (`PhotoQuery::default()`).
    pub photo_count: usize,
}

/// A `chairphoto://` link, resolved against the open catalog. The model holds the newest
/// one and emits it ([`AppModelEvent::DeepLink`]); the shell applies it.
///
/// | Link | Applied by |
/// |---|---|
/// | photo, view `grid` | the Library selects it, scope widened (`ShellState::apply_deep_link`) |
/// | photo, view `loupe` | … then opens the inline loupe on it |
/// | photo, view `develop` | … then opens the Darkroom (not ported yet, #111) |
/// | tag | the Library filters to the tag |
#[derive(Debug, Clone)]
pub enum DeepLinkTarget {
    /// `photo` is the whole row: a stacked child, which the grid never lists, is viewed
    /// off-grid from it.
    Photo { id: i64, uuid: String, path: String, view: DeepLinkView, photo: Box<Photo>, from: CatalogIdentity },
    Tag { id: i64, uuid: String, full_path: String },
}

/// App-wide state for the shell.
pub struct AppModel {
    state: AppState,
    /// Held for the app's lifetime; the image layer ([`crate::image_store`]) submits to it.
    pool: Option<Arc<ImagePool<Loaded>>>,
    /// The open catalog, once [`AppModel::refresh`] has read it.
    pub catalog: Option<CatalogSummary>,
    /// The identity of the catalog [`catalog`](Self::catalog) was read from, while it is the
    /// current one (`None` from a switch until its refresh lands). What a dialog that opens
    /// now binds its writes to (`with_catalog_as`), e.g. Preferences.
    identity: Option<CatalogIdentity>,
    /// One line on what the app is doing ("Opening catalog…", an error).
    pub status: SharedString,
    /// The last core event: its wire name and a short rendering of its payload.
    pub last_event: Option<SharedString>,
    /// How many core events have arrived.
    pub events_seen: u64,
    /// Bumped by every refresh; a result from an older one is dropped.
    generation: u64,
    /// Bumped by every `catalog:switched`: photo ids from before it mean other photos now,
    /// so whatever is keyed by them (the image cache) must be dropped.
    pub catalog_epoch: u64,
    /// The newest resolved `chairphoto://` link (see [`DeepLinkTarget`]).
    pub deep_link: Option<DeepLinkTarget>,
    /// Whether [`catalog`](Self::catalog) was read since the last catalog switch, so that a
    /// link resolved now asks the catalog the user sees. Kept apart from `catalog`, which the
    /// title bar goes on showing (the old name) until the switch's refresh lands.
    catalog_current: bool,
    /// The newest link not yet being resolved: one that arrived before the catalog was read
    /// (at startup, or since a switch), applied once it is (React's `ready`), or one that
    /// arrived while another resolution ran, started when that one finishes. One, not a
    /// queue: each link supersedes the one before, so of several waiting links only the newest
    /// could land; older ones are dropped as they are replaced.
    pending_link: Option<DeepLink>,
    /// The link whose resolution is running and still wanted, if any (a switch takes it back
    /// into `pending_link`).
    in_flight_link: Option<DeepLink>,
    /// Whether a resolution task is running. **At most one runs at a time**, so a flood of
    /// links from the single-instance socket costs one catalog lookup plus one waiting slot,
    /// not a lookup per link: the bounded request queue frees its slot as soon as a link is
    /// handed here, so the bound on the work has to be here.
    resolving: bool,
    /// How many resolutions have started (tests: the work is bounded).
    #[cfg(test)]
    resolutions_started: u64,
    /// Bumped by every link resolution and by every catalog switch; only the newest
    /// resolution's result lands, and never one started against a catalog since switched away.
    link_generation: u64,
}

impl EventEmitter<AppModelEvent> for AppModel {}

/// An external editor's setting changed (Preferences → Editors saved a path, RapidRAW's binary
/// or its format): which editors this machine has is worth re-checking
/// ([`AppModel::editors_changed`]; the inspector's "Edit in" list follows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorsChanged;

impl EventEmitter<EditorsChanged> for AppModel {}

impl AppModel {
    pub fn new(state: AppState, pool: Option<Arc<ImagePool<Loaded>>>) -> Self {
        Self {
            state,
            pool,
            catalog: None,
            identity: None,
            status: "Starting…".into(),
            last_event: None,
            events_seen: 0,
            generation: 0,
            catalog_epoch: 0,
            deep_link: None,
            catalog_current: false,
            pending_link: None,
            in_flight_link: None,
            resolving: false,
            #[cfg(test)]
            resolutions_started: 0,
            link_generation: 0,
        }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// See the field [`identity`](Self::identity).
    pub fn catalog_identity(&self) -> Option<CatalogIdentity> {
        self.identity
    }

    pub fn pool(&self) -> Option<&Arc<ImagePool<Loaded>>> {
        self.pool.as_ref()
    }

    /// Open the default catalog (`app::open_default_catalog`) on the core's runtime, then
    /// [`refresh`](Self::refresh). What React's `initCatalog()` on mount does.
    pub fn open_default_catalog(&mut self, cx: &mut Context<Self>) {
        self.status = "Opening catalog…".into();
        cx.notify();
        let state = self.state.clone();
        let opened = chairphoto_core::app::runtime()
            .spawn(async move { chairphoto_core::app::open_default_catalog(&state).await });
        cx.spawn(async move |this, cx| {
            let result = match opened.await {
                Ok(result) => result,
                Err(e) => Err(e.to_string()),
            };
            this.update(cx, |m, cx| {
                match result {
                    Ok(path) => {
                        eprintln!("catalog: opened {}", path.display());
                        m.status = "Catalog opened.".into();
                        m.refresh(cx);
                    }
                    Err(e) => {
                        eprintln!("catalog: failed to open the default catalog: {e}");
                        m.status = format!("Failed to open catalog: {e}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read everything catalog-derived this entity holds, off the UI thread. Only the
    /// newest refresh's result lands.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let state = self.state.clone();
        // Short catalog reads run on GPUI's background executor, not the core runtime's
        // blocking pool: GPUI's deterministic test scheduler rejects wakeups from foreign
        // threads, so only this path is testable headless. The cost (reviewed, gpui #99): while
        // a worker holds the catalog lock, each pending read parks one executor thread;
        // refreshes are event-driven and superseded ones are dropped, so few are in flight.
        // Long lock-holding jobs (scans, indexing) already run on the core runtime.
        let read = cx.background_executor().spawn(async move { read_summary(&state) });
        cx.spawn(async move |this, cx| {
            let summary = read.await;
            this.update(cx, |m, cx| {
                if m.generation != generation {
                    return; // superseded
                }
                match summary {
                    Ok((identity, summary)) => {
                        eprintln!("catalog: {} · {} photos", summary.name, summary.photo_count);
                        m.catalog = Some(summary);
                        m.identity = Some(identity);
                        m.catalog_current = true;
                        cx.emit(AppModelEvent::CatalogRead);
                        m.start_pending_link(cx);
                    }
                    Err(e) => {
                        m.catalog = None;
                        m.identity = None;
                        m.catalog_current = false;
                        m.status = format!("Catalog unavailable: {e}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A `chairphoto://` URL from the command line or a second launch. Anything that is not a
    /// ChairPhoto link is reported on the status line and dropped.
    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) {
        match deep_link::parse(url) {
            Some(link) => self.open_deep_link(link, cx),
            None => {
                eprintln!("deep link: ignored {url:?}");
                self.status = format!("Deep link: not a ChairPhoto link: {url}").into();
                cx.notify();
            }
        }
    }

    /// Resolve a parsed link against the open catalog, off the UI thread, and record it in
    /// [`deep_link`](Self::deep_link). Before the catalog is open — or after a switch, before
    /// the new catalog has been read — the link waits; so it does while another link resolves.
    /// A newer link supersedes an older one still resolving or waiting.
    pub fn open_deep_link(&mut self, link: DeepLink, cx: &mut Context<Self>) {
        if let Some(older) = self.pending_link.replace(link) {
            eprintln!("deep link: {older:?} superseded by a newer link before it was resolved");
        }
        if !self.catalog_current {
            self.status = "Deep link: waiting for the catalog…".into();
            cx.notify();
        }
        self.start_pending_link(cx);
    }

    /// Start resolving the waiting link, if there is one, the catalog has been read, and no
    /// other resolution runs (that one's completion calls this again).
    fn start_pending_link(&mut self, cx: &mut Context<Self>) {
        if !self.catalog_current || self.resolving {
            return;
        }
        let Some(link) = self.pending_link.take() else { return };
        self.resolving = true;
        #[cfg(test)]
        {
            self.resolutions_started += 1;
        }
        self.link_generation += 1;
        let generation = self.link_generation;
        let state = self.state.clone();
        self.in_flight_link = Some(link.clone());
        let read = cx.background_executor().spawn(async move { resolve_link(&state, &link) });
        cx.spawn(async move |this, cx| {
            let resolved = read.await;
            this.update(cx, |m, cx| {
                m.resolving = false;
                if m.link_generation != generation {
                    // The catalog was switched: the link went back to `pending_link`.
                } else if m.pending_link.is_some() {
                    m.in_flight_link = None; // superseded by the newer link that waits
                } else {
                    m.in_flight_link = None;
                    match resolved {
                        Ok(target) => {
                            m.status = link_status(&target).into();
                            m.deep_link = Some(target.clone());
                            cx.emit(AppModelEvent::DeepLink(target));
                        }
                        Err(message) => m.status = message.into(),
                    }
                    eprintln!("deep link: {}", m.status);
                    cx.notify();
                }
                m.start_pending_link(cx);
            })
            .ok();
        })
        .detach();
    }

    /// How many links wait for the catalog (tests: the queue is bounded).
    #[cfg(test)]
    pub(crate) fn pending_link_count(&self) -> usize {
        usize::from(self.pending_link.is_some())
    }

    /// How many link resolutions have started (tests: the work is bounded).
    #[cfg(test)]
    pub(crate) fn resolutions_started(&self) -> u64 {
        self.resolutions_started
    }

    /// A catalog switch: every photo and tag id from before it names something else now.
    ///
    /// - The resolved [`deep_link`](Self::deep_link) is dropped: its ids are the old catalog's.
    /// - A resolution still in flight can no longer land (`link_generation` moves on); it
    ///   may have read either catalog. It still counts as running (`resolving`) until it
    ///   finishes, so the next one starts only then: never two at once.
    /// - **Unresolved links carry over**: the in-flight one goes back to `pending_link`
    ///   (unless a newer one already waits there) and resolves against the new catalog once
    ///   its refresh lands. A `chairphoto://` URL names a photo or tag by uuid, not a catalog;
    ///   the user asked the app to show it, and the catalog the app has open when it can
    ///   answer is the one to ask. Dropping them would lose a click without a word.
    /// - **Until the new catalog is read, links wait** (`catalog_current`): a link arriving
    ///   now replaces the carried-over one in `pending_link`, so the refresh replays the
    ///   newest link, never an older one over a newer.
    fn on_catalog_switched(&mut self) {
        self.catalog_epoch += 1;
        self.catalog_current = false;
        self.identity = None;
        self.link_generation += 1;
        self.deep_link = None;
        if let Some(link) = self.in_flight_link.take() {
            self.pending_link.get_or_insert(link);
        }
        if self.pending_link.is_some() {
            self.status = "Deep link: waiting for the catalog…".into();
        }
    }

    /// A core event for this entity: note it, and refresh what it invalidates.
    pub fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        if let CoreEvent::CatalogSwitched(_) = event {
            self.on_catalog_switched();
        }
        let invalidates = match event {
            CoreEvent::CatalogSwitched(_) => true,
            CoreEvent::ScanProgress(p) => p.phase == "done",
            _ => false,
        };
        self.note_event(event.name(), payload_line(event), cx);
        if invalidates {
            self.refresh(cx);
        }
        cx.emit(AppModelEvent::Core(event.clone()));
    }

    /// A shell action whose feature a later ticket ports: say so in the status line rather
    /// than fake it (`shell::actions::NOT_YET_PORTED`).
    pub fn not_yet_ported(&mut self, what: &str, ticket: u32, cx: &mut Context<Self>) {
        eprintln!("shell: {what}: not yet ported (#{ticket})");
        self.status = not_yet_ported_line(what, ticket).into();
        cx.notify();
    }

    /// An external editor's setting was saved: say so ([`EditorsChanged`]).
    pub fn editors_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditorsChanged);
    }

    /// Put one line on the status line (the bench shows it).
    pub fn set_status(&mut self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = line.into();
        cx.notify();
    }

    /// Record `name payload` as the last event.
    pub fn note_event(&mut self, name: &str, payload: String, cx: &mut Context<Self>) {
        self.events_seen += 1;
        self.last_event = Some(format!("{name} {payload}").trim_end().to_string().into());
        cx.notify();
    }
}

/// The status line for a feature that is not ported yet.
pub fn not_yet_ported_line(what: &str, ticket: u32) -> String {
    format!("{what}: not yet ported to the GPUI app (#{ticket})")
}

/// The open catalog's name and photo count. Blocking (catalog lock + SQLite): background only.
fn read_summary(state: &AppState) -> Result<(CatalogIdentity, CatalogSummary), String> {
    with_catalog_identified(state, |c| {
        let photo_count = c.count_photos(&PhotoQuery::default())?;
        let path = c.db_path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        Ok(CatalogSummary { name, photo_count })
    })
}

/// Look a link's uuid up in the open catalog. `Err` is the status line to show, worded as
/// App.tsx worded it ("Deep link: no photo <uuid> in this catalog"). Blocking: background only.
fn resolve_link(state: &AppState, link: &DeepLink) -> Result<DeepLinkTarget, String> {
    match link {
        DeepLink::Photo { uuid, view } => {
            let (from, photo) = with_catalog_identified(state, |c| match c.get_photo_by_uuid(uuid) {
                Ok(p) => Ok(Some(p)),
                Err(CatalogError::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            })
            .map_err(|e| format!("Deep link: {e}"))?;
            let photo = photo.ok_or_else(|| format!("Deep link: no photo {uuid} in this catalog"))?;
            Ok(DeepLinkTarget::Photo {
                id: photo.id,
                uuid: photo.uuid.clone(),
                path: photo.path.clone(),
                view: *view,
                photo: Box::new(photo),
                from,
            })
        }
        DeepLink::Tag { uuid } => {
            // App.tsx matched against the loaded tag tree (`tags.find(t => t.uuid === uuid)`).
            let tags = with_catalog(state, |c| c.list_tags_with_counts()).map_err(|e| format!("Deep link: {e}"))?;
            let tag = tags
                .into_iter()
                .map(|t| t.tag)
                .find(|t| t.uuid == *uuid)
                .ok_or_else(|| format!("Deep link: no tag {uuid} in this catalog"))?;
            Ok(DeepLinkTarget::Tag { id: tag.id, uuid: tag.uuid, full_path: tag.full_path })
        }
    }
}

/// The status line for a resolved link: what it asks for. (Opening the loupe or the
/// Darkroom answers with its own not-yet-ported line once the photo is selected.)
fn link_status(target: &DeepLinkTarget) -> String {
    match target {
        DeepLinkTarget::Photo { path, view, .. } => {
            let surface = match view {
                DeepLinkView::Grid => "Library",
                DeepLinkView::Loupe => "loupe",
                DeepLinkView::Develop => "Darkroom",
            };
            format!("Deep link: {path} → {surface}")
        }
        DeepLinkTarget::Tag { full_path, .. } => format!("Deep link: filter by tag {full_path}"),
    }
}

/// The event's payload as compact JSON, cut to one readable line.
fn payload_line(event: &CoreEvent) -> String {
    struct Json(std::cell::RefCell<String>);
    impl EventVisitor for Json {
        fn visit<T: serde::Serialize + Clone>(&self, _name: &'static str, payload: &T) {
            *self.0.borrow_mut() = serde_json::to_string(payload).unwrap_or_default();
        }
    }
    let json = Json(Default::default());
    event.visit(&json);
    let mut line = json.0.into_inner();
    const MAX: usize = 160;
    if line.chars().count() > MAX {
        line = line.chars().take(MAX).collect::<String>() + "…";
    }
    line
}
