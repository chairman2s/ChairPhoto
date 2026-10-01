//! [`MapState`]: the Map module's catalog-derived data, shared by its views (the map, the
//! settings panel, the inspector's Geocode panel) — GPS points, fences, the tile source and
//! the per-host tile consent — and every catalog write the module makes.
//!
//! **Reads and writes run off the UI thread** on the storage [`Runner`] (the core runtime's
//! blocking pool; a manual queue in tests). Every result carries the generation it was
//! started under; a catalog switch bumps it, so a read or write that lands after a switch is
//! dropped, never applied to the new catalog's state. The data reloads on every catalog
//! read the app model announces (startup, a switch, a finished scan) and after the module's
//! own writes.
//!
//! **Catalog identity.** The switch publishes the new catalog before `catalog:switched`
//! reaches this state, so a write keyed by ids read earlier (a fence id, a photo id) could
//! land on the new catalog's rows. Each read records the [`CatalogIdentity`] it came from;
//! every write — fence create/update/delete/apply, the inspector's geocode and Geocode all —
//! runs through `with_catalog_as` with it and fails closed with `CATALOG_CHANGED` once
//! another catalog is open. Before the first read lands (or after a switch) there is no
//! identity and a write is refused with a status line.
//!
//! **Consent** (decision #118) is a catalog setting, `map.tileHosts`, so it is remembered
//! per catalog: another catalog asks again. Until it has been read, the host counts as
//! "never asked" and nothing is fetched.

use super::logic::{Consent, HostConsent, TILE_HOSTS_KEY, TILE_URL_KEY};
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::storage::Runner;
use chairphoto_core::app::{with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::plugins::map::cluster::{project_points, ProjectedPoint};
use chairphoto_core::plugins::map::tiles::TileSource;
use chairphoto_core::plugins::map::{self as backend, Fence, LatLng};
use gpui_kit::{Context, Entity, Subscription};
use std::sync::Arc;

/// Whether a read has landed.
#[derive(Debug, Clone, PartialEq)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// What the module stores in its settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapSettingsData {
    /// The stored `map.tileUrl` (raw), if any.
    pub tile_url: Option<String>,
    pub consent: HostConsent,
}

/// Where "Geocode all" stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeocodeRun {
    pub busy: bool,
    pub done: usize,
    pub total: usize,
    pub filled: usize,
    pub status: String,
}

pub struct MapState {
    app: AppState,
    settings: ModuleSettings,
    model: Entity<AppModel>,
    /// GPS points, projected once (the clusterer's input).
    pub points: Load<Arc<Vec<ProjectedPoint>>>,
    /// Bumped whenever `points` is replaced, so views re-cluster and re-fit.
    pub points_revision: u64,
    /// The catalog `points` were read from (photo ids: the inspector's geocode, Geocode all).
    points_from: Option<CatalogIdentity>,
    pub fences: Vec<Fence>,
    /// The catalog `fences` were read from (fence ids: every fence write).
    fences_from: Option<CatalogIdentity>,
    pub fence_error: Option<String>,
    /// `None` until the settings have been read.
    pub stored: Option<MapSettingsData>,
    pub source: TileSource,
    /// Why the stored tile URL was refused (the default is used meanwhile).
    pub source_error: Option<String>,
    /// The fence being applied (`Some(id)`), or all of them (`Some(None)`).
    pub applying: Option<Option<i64>>,
    pub geocode: GeocodeRun,
    /// Bumped by every catalog switch: results from before it are dropped.
    generation: u64,
    _model: Subscription,
}

impl MapState {
    pub fn new(app: AppState, settings: ModuleSettings, model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let _model = cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
            AppModelEvent::CatalogRead => this.reload(cx),
            AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
            AppModelEvent::Core(CoreEvent::GeocodeProgress(p)) => {
                if this.geocode.busy {
                    this.geocode.done = p.done;
                    this.geocode.total = p.total;
                    this.geocode.filled = p.filled;
                    this.geocode.status = format!("{} / {} processed, {} filled…", p.done, p.total, p.filled);
                    cx.notify();
                }
            }
            _ => {}
        });
        let mut this = MapState {
            app,
            settings,
            model: model.clone(),
            points: Load::Loading,
            points_revision: 0,
            points_from: None,
            fences: Vec::new(),
            fences_from: None,
            fence_error: None,
            stored: None,
            source: TileSource::default(),
            source_error: None,
            applying: None,
            geocode: GeocodeRun::default(),
            generation: 0,
            _model,
        };
        // Enabled after the model's first catalog read (the usual case: the registry restores
        // modules on it): read now rather than wait for the next one.
        if model.read(cx).catalog.is_some() {
            this.reload(cx);
        }
        this
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn app(&self) -> &AppState {
        &self.app
    }

    /// The catalog the shown photos were read from: what a write keyed by a photo id the
    /// user sees now is bound to. `None` until a read lands (and after a switch).
    pub fn catalog(&self) -> Option<CatalogIdentity> {
        self.points_from
    }

    /// The consent for the current tile host. `Unknown` until the settings are read.
    pub fn consent(&self) -> Consent {
        self.stored.as_ref().map_or(Consent::Unknown, |s| s.consent.get(self.source.host()))
    }

    /// Whether the settings have been read (the consent question can be asked).
    pub fn settings_known(&self) -> bool {
        self.stored.is_some()
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.points = Load::Loading;
        self.points_revision += 1;
        self.points_from = None;
        self.fences.clear();
        self.fences_from = None;
        self.fence_error = None;
        self.stored = None;
        self.source = TileSource::default();
        self.source_error = None;
        self.applying = None;
        self.geocode = GeocodeRun::default();
        cx.notify();
    }

    /// Run `work` off the UI thread and hand its result to `land` — unless the catalog was
    /// switched meanwhile.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&AppState, &ModuleSettings) -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let generation = self.generation;
        let (app, settings) = (self.app.clone(), self.settings.clone());
        let rx = Runner::get(cx).run(move || work(&app, &settings));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.generation != generation {
                    return; // a catalog switch came between: this is the old catalog's
                }
                land(s, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read points, fences and settings.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_points(cx);
        self.reload_fences(cx);
        self.reload_settings(cx);
    }

    pub fn reload_points(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |app, _| {
                with_catalog_identified(app, |c| {
                    backend::ensure_schema_for(c)?;
                    Ok(backend::map_photo_points_for(c)?)
                })
                .map(|(from, points)| (from, project_points(&points))) // projected off the UI thread too
            },
            |s, result, _| {
                s.points = match result {
                    Ok((from, points)) => {
                        s.points_from = Some(from);
                        Load::Ready(Arc::new(points))
                    }
                    Err(e) => Load::Failed(e),
                };
                s.points_revision += 1;
            },
        );
    }

    pub fn reload_fences(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |app, _| {
                with_catalog_identified(app, |c| {
                    backend::ensure_schema_for(c)?;
                    Ok(backend::list_fences_for(c)?)
                })
            },
            |s, result, _| match result {
                Ok((from, f)) => {
                    s.fences_from = Some(from);
                    s.fences = f;
                    s.fence_error = None;
                }
                Err(e) => s.fence_error = Some(e),
            },
        );
    }

    pub fn reload_settings(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |_, settings| -> Result<MapSettingsData, String> {
                Ok(MapSettingsData {
                    tile_url: settings.get(TILE_URL_KEY)?,
                    consent: HostConsent::parse(settings.get(TILE_HOSTS_KEY)?.as_deref()),
                })
            },
            |s, result, _| match result {
                Ok(data) => s.apply_settings(data),
                Err(e) => eprintln!("map: settings unavailable: {e}"),
            },
        );
    }

    fn apply_settings(&mut self, data: MapSettingsData) {
        match TileSource::parse(data.tile_url.as_deref().unwrap_or_default()) {
            Ok(source) => {
                self.source = source;
                self.source_error = None;
            }
            Err(e) => {
                self.source = TileSource::default();
                self.source_error = Some(format!("The saved tile URL is not usable ({e}); using OpenStreetMap."));
            }
        }
        self.stored = Some(data);
    }

    /// Remember the answer for `host` (the consent prompt, or Preferences).
    pub fn set_consent(&mut self, host: &str, allowed: Option<bool>, cx: &mut Context<Self>) {
        let Some(stored) = self.stored.as_mut() else { return };
        match allowed {
            Some(a) => stored.consent.set(host, a),
            None => stored.consent.forget(host),
        }
        let json = stored.consent.to_json();
        cx.notify();
        self.run(cx, move |_, settings| settings.set(TILE_HOSTS_KEY, &json), |_, r, _| {
            if let Err(e) = r {
                eprintln!("map: could not save the tile consent: {e}");
            }
        });
    }

    /// Save a tile URL (empty = the default). Refused with the reason when unusable.
    pub fn set_tile_url(&mut self, url: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let source = TileSource::parse(url)?;
        let stored = source.template().to_string();
        self.source = source;
        self.source_error = None;
        if let Some(s) = self.stored.as_mut() {
            s.tile_url = Some(stored.clone());
        }
        cx.notify();
        self.run(cx, move |_, settings| settings.set(TILE_URL_KEY, &stored), |_, r, _| {
            if let Err(e) = r {
                eprintln!("map: could not save the tile URL: {e}");
            }
        });
        Ok(())
    }

    fn status(&self, line: String, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            m.status = line.into();
            cx.notify();
        });
    }

    /// Photos' tags changed: every catalog-derived entity re-reads (api.ts's rule for a
    /// mutation, `notifyChange` in React).
    pub(crate) fn catalog_changed(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    /// The catalog the fences were read from, or — before one has — a status line and `None`.
    fn fence_catalog(&self, cx: &mut Context<Self>) -> Option<CatalogIdentity> {
        if self.fences_from.is_none() {
            self.status("The catalog's fences are still loading; try again.".into(), cx);
        }
        self.fences_from
    }

    /// Draw-and-save: into the catalog whose fences the map shows.
    pub fn create_fence(&mut self, name: String, tag_path: String, polygon: Vec<LatLng>, cx: &mut Context<Self>) {
        let Some(from) = self.fence_catalog(cx) else { return };
        self.run(
            cx,
            move |app, _| {
                with_catalog_as(app, from, |c| {
                    backend::ensure_schema_for(c)?;
                    Ok(backend::create_fence_for(c, &name, &tag_path, &polygon)?)
                })
            },
            |s, r, cx| match r {
                Ok(_) => s.reload_fences(cx),
                Err(e) => s.status(format!("Failed to save fence: {e}"), cx),
            },
        );
    }

    /// Save a fence's name, tag path and polygon. The local copy changes at once (a dragged
    /// vertex must not snap back while the write runs); a failure reloads the stored one.
    pub fn update_fence(&mut self, fence: Fence, cx: &mut Context<Self>) {
        let Some(from) = self.fence_catalog(cx) else { return };
        if let Some(f) = self.fences.iter_mut().find(|f| f.id == fence.id) {
            *f = fence.clone();
        }
        cx.notify();
        self.run(
            cx,
            move |app, _| {
                with_catalog_as(app, from, |c| {
                    backend::ensure_schema_for(c)?;
                    Ok(backend::update_fence_for(c, fence.id, &fence.name, &fence.tag_path, &fence.polygon)?)
                })
            },
            |s, r, cx| {
                if let Err(e) = r {
                    s.status(format!("Failed to update fence: {e}"), cx);
                    s.reload_fences(cx);
                }
            },
        );
    }

    pub fn delete_fence(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(from) = self.fence_catalog(cx) else { return };
        self.run(
            cx,
            move |app, _| with_catalog_as(app, from, |c| Ok(backend::delete_fence_for(c, id)?)),
            |s, r, cx| match r {
                Ok(_) => s.reload_fences(cx),
                Err(e) => s.status(format!("Failed to delete fence: {e}"), cx),
            },
        );
    }

    /// Apply one fence (`Some(id)`) or all (`None`): a count on the status line, and every
    /// catalog-derived entity re-reads. One apply at a time.
    pub fn apply(&mut self, fence: Option<i64>, cx: &mut Context<Self>) {
        if self.applying.is_some() {
            return;
        }
        let Some(from) = self.fence_catalog(cx) else { return };
        self.applying = Some(fence);
        let name = fence.and_then(|id| self.fences.iter().find(|f| f.id == id)).map(|f| f.name.clone());
        cx.notify();
        self.run(
            cx,
            move |app, _| {
                with_catalog_as(app, from, |c| {
                    backend::ensure_schema_for(c)?;
                    match fence {
                        Some(id) => backend::apply_fence(c, id),
                        None => backend::apply_all_fences(c),
                    }
                })
            },
            move |s, r, cx| {
                s.applying = None;
                match r {
                    Ok(n) => {
                        let photos = if n == 1 { "1 photo".to_string() } else { format!("{n} photos") };
                        let line = match &name {
                            Some(name) => format!("Applied \u{201c}{name}\u{201d}: {photos} newly tagged."),
                            None => format!("Applied all fences: {photos} newly tagged."),
                        };
                        s.status(line, cx);
                        s.catalog_changed(cx);
                    }
                    Err(e) => s.status(format!("Failed to apply fence: {e}"), cx),
                }
            },
        );
    }

    /// "Geocode all with GPS": Nominatim, ≤ 1 request/s, on the core runtime (network, not a
    /// blocking worker); progress arrives as `geocode:progress` through the model.
    pub fn geocode_all(&mut self, cx: &mut Context<Self>) {
        if self.geocode.busy {
            return;
        }
        let Some(from) = self.points_from else {
            self.status("The catalog is still loading; try again.".into(), cx);
            return;
        };
        self.geocode = GeocodeRun { busy: true, status: "Starting…".into(), ..Default::default() };
        cx.notify();
        let (generation, app) = (self.generation, self.app.clone());
        let task = chairphoto_core::app::runtime()
            .spawn(async move { backend::geocode::geocode_all_to_iptc(&app, Some(from)).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |s, cx| {
                if s.generation != generation {
                    return;
                }
                let line = match result {
                    Ok(sum) if sum.total == 0 => "No photos with GPS and empty location fields.".to_string(),
                    Ok(sum) => {
                        if sum.filled > 0 {
                            s.catalog_changed(cx);
                        }
                        let mut line = format!("Done: {} of {} photos had location fields filled.", sum.filled, sum.total);
                        if sum.skipped > 0 {
                            line.push_str(&format!(" {} already set or no result.", sum.skipped));
                        }
                        line
                    }
                    Err(e) => format!("Geocode failed: {e}"),
                };
                s.geocode = GeocodeRun { status: line.clone(), ..Default::default() };
                s.status(line, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
