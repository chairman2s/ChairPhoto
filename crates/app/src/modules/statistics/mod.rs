//! The Statistics module (`src/modules/plugins/statistics.tsx`): a main view, "Stats", with
//! a read-only report on the catalog or the sidebar's tag/album/batch scope — stat cards,
//! facts, the timeline, the 24 h clock, weekday/focal/rating/exposure bars, the camera donut,
//! rank lists, cull survival, the ≥ 4★ hit rate and the keeper analysis. A top-tag row
//! filters the Library by that tag (`api.filterByTag`).
//!
//! **Where the logic lives.** Everything but the drawing is `chairphoto_model::statistics`:
//! the fetch session (generation-tagged requests, the 16-scope cache, the epoch that a
//! catalog switch or unload bumps) and [`Dashboard::derive`]. This module owns the
//! [`Statistics`] entity that runs the reads, and the view ([`view::StatisticsView`]) draws
//! the dashboard with gpui-component's charts (area, pie, bar) and plain rows.
//!
//! **When it reads.** React fetched on every mount and on every scope change while
//! mounted. Here the view is cached while the module stays enabled, so the entity watches
//! the shell instead: a read starts when the stage switches to the Stats view, and when the
//! Library scope changes while it shows. Each read runs on the background executor
//! (`Catalog::catalog_stats` holds the catalog lock for one full pass over the photos);
//! only the newest one's answer is shown.
//!
//! **Catalog switch and unload** reset the session: the cache and the figures go, and an
//! answer still in flight from before is dropped (`StatsSession::reset`), so figures from a
//! closed catalog can never land.
//!
//! No backend feature (`catalog_stats` is core, in every build) and no settings.

pub mod view;

#[cfg(test)]
mod tests;

use super::{view as view_factory, Contributions, MainView, Module, ModuleHost, ModuleInstance, ModuleMeta};
use crate::shell::state::Surface;
use crate::shell::ShellState;
use chairphoto_core::app::{with_catalog, AppState, CoreEvent};
use chairphoto_core::catalog::CatalogStatsRaw;
use chairphoto_model::statistics::{Dashboard, RateMetric, StatsRequest, StatsScope, StatsSession};
use gpui_kit::component::Icon;
use gpui_kit::{App, AppContext as _, Context, Entity, Subscription};
use std::rc::Rc;

pub const STATISTICS_ID: &str = "statistics";
/// The main view's id: what `Surface::Module` names and the rail orders by.
pub const STATISTICS_VIEW_ID: &str = "statistics";

pub struct StatisticsModule;

impl Module for StatisticsModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(STATISTICS_ID, "Statistics")
            .description("Dashboard of catalog stats — timeline, top tags, cameras, lenses, and shooting habits.")
    }

    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        let shell = host.shell().clone();
        let state = cx.new(|cx| Statistics::new(app, shell, cx));
        Ok(Box::new(StatisticsInstance { state }))
    }
}

struct StatisticsInstance {
    state: Entity<Statistics>,
}

impl ModuleInstance for StatisticsInstance {
    fn contributions(&self) -> Contributions {
        let state = self.state.clone();
        Contributions {
            main_views: vec![MainView {
                id: STATISTICS_VIEW_ID.into(),
                label: "Stats".into(),
                icon: Some(Icon::new(gpui_kit::assets::IconName::ChartNoAxesColumn)),
                view: view_factory(move |_, cx| view::StatisticsView::new(state.clone(), cx)),
            }],
            ..Default::default()
        }
    }

    fn on_event(&mut self, event: &CoreEvent, cx: &mut App) {
        if let CoreEvent::CatalogSwitched(_) = event {
            self.state.update(cx, |s, cx| s.catalog_switched(cx));
        }
    }

    fn on_unload(&mut self, cx: &mut App) {
        self.state.update(cx, |s, cx| s.unload(cx));
    }
}

/// The module's live state: the fetch session, the rate metric and the derived dashboard.
pub struct Statistics {
    app: AppState,
    shell: Entity<ShellState>,
    session: StatsSession,
    metric: RateMetric,
    /// The Stats view is on the stage.
    visible: bool,
    /// The scope of the newest read.
    requested: Option<StatsScope>,
    /// The dashboard derived from the figures on screen and `metric`, kept until either
    /// changes (a long timeline is thousands of points).
    derived: Option<(Rc<CatalogStatsRaw>, RateMetric, Rc<Dashboard>)>,
    /// Off after `on_unload`: nothing more is read.
    live: bool,
    _shell: Subscription,
}

impl Statistics {
    fn new(app: AppState, shell: Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let _shell = cx.observe(&shell, |this, _, cx| this.sync(cx));
        let mut this = Statistics {
            app,
            shell,
            session: StatsSession::new(),
            metric: RateMetric::default(),
            visible: false,
            requested: None,
            derived: None,
            live: true,
            _shell,
        };
        this.sync(cx);
        this
    }

    /// Follow the shell: read when the Stats view comes on stage, or the scope changes
    /// while it is there.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (visible, scope) = {
            let shell = self.shell.read(cx);
            let visible = matches!(&shell.surface, Surface::Module(id) if id == STATISTICS_VIEW_ID);
            (visible, StatsScope::from_library(shell.library.scope()))
        };
        let appeared = visible && !self.visible;
        self.visible = visible;
        if self.live && visible && (appeared || self.requested != Some(scope)) {
            self.fetch(scope, cx);
        }
    }

    fn fetch(&mut self, scope: StatsScope, cx: &mut Context<Self>) {
        let request = self.session.request(scope);
        self.requested = Some(scope);
        let state = self.app.clone();
        let read = cx.background_executor().spawn(async move {
            with_catalog(&state, |c| c.catalog_stats(scope.tag_id, scope.album_id, scope.batch_id))
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |s, cx| s.land(&request, result, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn land(&mut self, request: &StatsRequest, result: Result<CatalogStatsRaw, String>, cx: &mut Context<Self>) {
        let failed = result.as_ref().err().cloned();
        if self.session.apply(request, result) {
            if let Some(e) = failed {
                eprintln!("statistics: catalog_stats failed: {e}");
            }
            cx.notify();
        }
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.session.reset();
        self.requested = None;
        self.derived = None;
        // No read here: the Library scope may still name the old catalog's tag until the
        // shell has handled the switch, which also sends the stage back to the Library. The
        // next time the Stats view comes on stage, it reads the new catalog.
        self.visible = false;
        cx.notify();
    }

    fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.session.reset();
        self.requested = None;
        self.derived = None;
        cx.notify();
    }

    /// Keep rate or ≥ 4★ hit rate for the keeper-analysis crossings.
    pub fn set_metric(&mut self, metric: RateMetric, cx: &mut Context<Self>) {
        if self.metric != metric {
            self.metric = metric;
            cx.notify();
        }
    }

    pub fn metric(&self) -> RateMetric {
        self.metric
    }

    pub fn session(&self) -> &StatsSession {
        &self.session
    }

    /// The dashboard for the figures on screen, derived once per result and metric.
    pub fn dashboard(&mut self) -> Option<Rc<Dashboard>> {
        let stats = self.session.stats()?.clone();
        if let Some((s, m, d)) = &self.derived {
            if Rc::ptr_eq(s, &stats) && *m == self.metric {
                return Some(d.clone());
            }
        }
        let d = Rc::new(Dashboard::derive(&stats, self.metric));
        self.derived = Some((stats, self.metric, d.clone()));
        Some(d)
    }

    /// The scope chip: the scope of the newest read, named from the shell's scope info.
    pub fn scope_label(&self, cx: &App) -> Option<String> {
        let scope = self.requested?;
        let shell = self.shell.read(cx);
        scope.label(shell.scope_info.tag_name.as_deref())
    }

    /// A top-tag row: the Library, filtered by that tag (`api.filterByTag`).
    pub fn filter_by_tag(this: &Entity<Self>, tag_id: i64, cx: &mut App) {
        let shell = this.read(cx).shell.clone();
        shell.update(cx, |s, cx| {
            s.update_scope(cx, |l| l.select_tag(Some(tag_id)));
            s.show_library(cx);
        });
    }
}
