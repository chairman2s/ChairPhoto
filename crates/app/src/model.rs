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

use chairphoto_core::app::{with_catalog, AppState, CoreEvent, EventVisitor};
use chairphoto_core::catalog::{CatalogError, PhotoQuery};
use chairphoto_core::image_pool::ImagePool;
use chairphoto_model::deep_link::{self, DeepLink, DeepLinkView};
use gpui_kit::{Context, SharedString};
use std::sync::Arc;

/// What the shell shows about the open catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSummary {
    /// The catalog file's name, as the React title bar shows it (`default.chairphoto`).
    pub name: String,
    /// Every photo in the library view with no filter (`PhotoQuery::default()`).
    pub photo_count: usize,
}

/// A `chairphoto://` link, resolved against the open catalog: what the Library view, the
/// loupe and the Darkroom apply once they are ported. Until then the model holds the newest
/// one and says so on the status line.
///
/// | Link | Applied by (not ported yet) |
/// |---|---|
/// | photo, view `grid` | the Library view selects it, scope widened (#106) |
/// | photo, view `loupe` | … then opens the inline loupe (#109) |
/// | photo, view `develop` | … then opens the Darkroom (#111) |
/// | tag | the Library filters to the tag (#106) |
#[derive(Debug, Clone, PartialEq)]
pub enum DeepLinkTarget {
    Photo { id: i64, uuid: String, path: String, view: DeepLinkView },
    Tag { id: i64, uuid: String, full_path: String },
}

/// App-wide state for the shell.
pub struct AppModel {
    state: AppState,
    /// Held for the app's lifetime; the image layer (#101) submits to it.
    pool: Option<Arc<ImagePool>>,
    /// The open catalog, once [`AppModel::refresh`] has read it.
    pub catalog: Option<CatalogSummary>,
    /// One line on what the app is doing ("Opening catalog…", an error).
    pub status: SharedString,
    /// The last core event: its wire name and a short rendering of its payload.
    pub last_event: Option<SharedString>,
    /// How many core events have arrived.
    pub events_seen: u64,
    /// Bumped by every refresh; a result from an older one is dropped.
    generation: u64,
    /// The newest resolved `chairphoto://` link (see [`DeepLinkTarget`]).
    pub deep_link: Option<DeepLinkTarget>,
    /// Links that arrived before the catalog was open, applied once it is (React's `ready`).
    pending_links: Vec<DeepLink>,
    /// Bumped by every link resolution; only the newest link's result lands.
    link_generation: u64,
}

impl AppModel {
    pub fn new(state: AppState, pool: Option<Arc<ImagePool>>) -> Self {
        Self {
            state,
            pool,
            catalog: None,
            status: "Starting…".into(),
            last_event: None,
            events_seen: 0,
            generation: 0,
            deep_link: None,
            pending_links: Vec::new(),
            link_generation: 0,
        }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn pool(&self) -> Option<&Arc<ImagePool>> {
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
                    Ok(summary) => {
                        eprintln!("catalog: {} · {} photos", summary.name, summary.photo_count);
                        m.catalog = Some(summary);
                        for link in std::mem::take(&mut m.pending_links) {
                            m.open_deep_link(link, cx);
                        }
                    }
                    Err(e) => {
                        m.catalog = None;
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
    /// [`deep_link`](Self::deep_link). Before the catalog is open the link waits; a newer link
    /// supersedes an older one still resolving.
    pub fn open_deep_link(&mut self, link: DeepLink, cx: &mut Context<Self>) {
        if self.catalog.is_none() {
            self.pending_links.push(link);
            self.status = "Deep link: waiting for the catalog…".into();
            cx.notify();
            return;
        }
        self.link_generation += 1;
        let generation = self.link_generation;
        let state = self.state.clone();
        let read = cx.background_executor().spawn(async move { resolve_link(&state, &link) });
        cx.spawn(async move |this, cx| {
            let resolved = read.await;
            this.update(cx, |m, cx| {
                if m.link_generation != generation {
                    return; // superseded
                }
                match resolved {
                    Ok(target) => {
                        m.status = link_status(&target).into();
                        m.deep_link = Some(target);
                    }
                    Err(message) => m.status = message.into(),
                }
                eprintln!("deep link: {}", m.status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A core event for this entity: note it, and refresh what it invalidates.
    pub fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        let invalidates = match event {
            CoreEvent::CatalogSwitched(_) => true,
            CoreEvent::ScanProgress(p) => p.phase == "done",
            _ => false,
        };
        self.note_event(event.name(), payload_line(event), cx);
        if invalidates {
            self.refresh(cx);
        }
    }

    /// Record `name payload` as the last event.
    pub fn note_event(&mut self, name: &str, payload: String, cx: &mut Context<Self>) {
        self.events_seen += 1;
        self.last_event = Some(format!("{name} {payload}").trim_end().to_string().into());
        cx.notify();
    }
}

/// The open catalog's name and photo count. Blocking (catalog lock + SQLite): background only.
fn read_summary(state: &AppState) -> Result<CatalogSummary, String> {
    with_catalog(state, |c| {
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
            let photo = with_catalog(state, |c| match c.get_photo_by_uuid(uuid) {
                Ok(p) => Ok(Some(p)),
                Err(CatalogError::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            })
            .map_err(|e| format!("Deep link: {e}"))?;
            let photo = photo.ok_or_else(|| format!("Deep link: no photo {uuid} in this catalog"))?;
            Ok(DeepLinkTarget::Photo { id: photo.id, uuid: photo.uuid, path: photo.path, view: *view })
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

/// The status line for a resolved link: what it asks for, and that the view that applies it
/// is not ported yet.
fn link_status(target: &DeepLinkTarget) -> String {
    match target {
        DeepLinkTarget::Photo { path, view, .. } => {
            let surface = match view {
                DeepLinkView::Grid => "Library",
                DeepLinkView::Loupe => "loupe",
                DeepLinkView::Develop => "Darkroom",
            };
            format!("Deep link: {path} → {surface} (view not ported yet)")
        }
        DeepLinkTarget::Tag { full_path, .. } => {
            format!("Deep link: filter by tag {full_path} (view not ported yet)")
        }
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
