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
use chairphoto_core::catalog::PhotoQuery;
use chairphoto_core::image_pool::ImagePool;
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
