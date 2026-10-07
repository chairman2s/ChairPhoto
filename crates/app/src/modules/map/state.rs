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
//! **Consent** (decision #118: per host, remembered, changeable in Preferences) is a
//! per-machine preference ([`MachinePrefs`] key `map.tileHosts`): a tile request reveals
//! this computer's address, whichever catalog is open, so a catalog switch does not ask
//! again. A catalog's old per-catalog answers are merged in on its first read
//! ([`MapState::reload_settings`]). The tile URL stays a catalog setting (`map.tileUrl`, as
//! in React): consent is keyed by the host the URL names, so it need not move with it.
//! Until a catalog's settings are read the host is not known and nothing is fetched.
//!
//! **Fail closed on an undurable store** (#214). [`MachinePrefs`] is the only copy of this
//! machine's answers; this state reads them from it ([`MapState::host_consent`]) and keeps
//! none of its own. The user's own answers ([`MapState::set_consent`]) are immediate changes:
//! applied at once, on any store. A catalog's legacy answers are merged in by a durable
//! change ([`MapState::migrate_consent`], [`MachinePrefs::modify_durably`]): computed from the
//! answers as they are when its write runs, visible only once that write is on disk, and
//! never applied on a store that cannot write — in memory only (no app data dir) or failing.
//! Otherwise a restart, which finds the store empty again, would replay the merge forever,
//! re-allowing a host the user reset in a session just as undurable. The catalog's copy is
//! emptied only after the merge is on disk, and the merge records the catalog as merged in
//! that same write, so a copy whose emptying failed is not merged again (#229) — until a read
//! confirms the emptying landed, after which a copy that reappears with content is new, and
//! only its Denies are folded in, for hosts with no answer — never over an Allow (#231, relD
//! M1). A host the user answers while a merge is in flight
//! is left out of it ([`ConsentAnswers`]), so the user's answer wins. A write that fails is
//! surfaced: [`MapState::consent_write_error`], which Preferences' Map tab shows.

use super::logic::{Consent, HostConsent, MACHINE_TILE_HOSTS, TILE_HOSTS_KEY, TILE_URL_KEY};
use crate::machine_prefs::MachinePrefs;
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::storage::Runner;
use chairphoto_core::app::{with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::plugins::map::cluster::{project_points, ProjectedPoint};
use chairphoto_core::plugins::map::tiles::TileSource;
use chairphoto_core::plugins::map::{self as backend, Fence, LatLng};
use chairphoto_core::app::GeocodeProgress;
use chairphoto_core::plugins::map::geocode::GeocodeAllSummary;
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::{App, Context, Entity, Global, Subscription};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

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
}

/// The hosts the user answered ([`MapState::set_consent`]) and in which order, for this app
/// run (a GPUI global, so it outlives a module reload). A legacy merge notes the sequence
/// number when it starts and leaves out every host answered after it, so the user's answer
/// wins over a merge in flight (#214).
#[derive(Default)]
pub(crate) struct ConsentAnswers(Arc<Mutex<AnswerLog>>);

impl Global for ConsentAnswers {}

#[derive(Default)]
struct AnswerLog {
    seq: u64,
    /// Each host's latest answer's sequence number.
    at: BTreeMap<String, u64>,
}

impl AnswerLog {
    fn answer(&mut self, host: &str) {
        self.seq += 1;
        self.at.insert(host.to_string(), self.seq);
    }

    fn answered_after(&self, host: &str, since: u64) -> bool {
        self.at.get(host).is_some_and(|&at| at > since)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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
    /// For its key names (`map.<key>`). This state outlives catalog switches, so its reads
    /// and writes do not go through the handle (bound to the catalog open when the module
    /// loaded): they capture the identity with each read (`settings_from`) and write through
    /// `with_catalog_as`, which is what `ModuleSettings` itself does for one catalog opening.
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
    /// The catalog `stored` was read from (the tile URL is saved there).
    settings_from: Option<CatalogIdentity>,
    /// The most recent machine-prefs write for tile consent that failed (#214), surfaced in
    /// Preferences' Map tab. Cleared by the next write that succeeds.
    consent_write_error: Option<String>,
    pub source: TileSource,
    /// Why the stored tile URL was refused (the default is used meanwhile).
    pub source_error: Option<String>,
    /// The fence being applied (`Some(id)`), or all of them (`Some(None)`).
    pub applying: Option<Option<i64>>,
    pub geocode: GeocodeRun,
    /// The Geocode all run in flight, if any: its owner's handle (at most one).
    geocode_job: Option<GeocodeJob>,
    /// The last Geocode all run id handed out.
    geocode_seq: u64,
    /// Bumped by every catalog switch: results from before it are dropped.
    generation: u64,
    _model: Subscription,
}

/// The owner's handle on one Geocode all run. Progress and the result land only while
/// this run is still `geocode_job`; [`stop`](Self::stop) makes it unreachable and stops it.
struct GeocodeJob {
    id: u64,
    /// Checked by the run before each photo and before each write.
    abort: Arc<AtomicBool>,
    ticket: Box<dyn GeocodeTicket>,
}

impl GeocodeJob {
    fn stop(self) {
        self.abort.store(true, Ordering::SeqCst);
        self.ticket.cancel();
    }
}

/// How one Geocode all run ends: the summary, or why not.
pub type GeocodeDone = futures::channel::oneshot::Sender<Result<GeocodeAllSummary, String>>;

/// A run in progress; cancelling it stops the work at once.
pub trait GeocodeTicket {
    fn cancel(&self);
}

/// Where Geocode all runs: [`NetGeocode`] in the app, a recording fake in tests (GPUI's test
/// scheduler rejects wakeups from the core runtime's threads, as [`Runner`] explains).
pub trait GeocodeBackend: Send + Sync {
    /// Run `geocode_all_to_iptc_with` bound to `from`, stopping when `abort` is set; each
    /// photo's progress goes to `progress`, the end to `done`.
    fn run(
        &self,
        app: AppState,
        from: CatalogIdentity,
        abort: Arc<AtomicBool>,
        progress: UnboundedSender<GeocodeProgress>,
        done: GeocodeDone,
    ) -> Box<dyn GeocodeTicket>;
}

/// The installed backend, a GPUI global; absent means [`NetGeocode`].
#[derive(Clone)]
pub struct MapGeocode(pub Arc<dyn GeocodeBackend>);

impl Global for MapGeocode {}

impl MapGeocode {
    pub fn get(cx: &App) -> Arc<dyn GeocodeBackend> {
        cx.try_global::<MapGeocode>().map_or_else(|| Arc::new(NetGeocode) as Arc<dyn GeocodeBackend>, |g| g.0.clone())
    }
}

/// The real backend: the core runtime (network, not a blocking worker).
pub struct NetGeocode;

struct AbortTask(tokio::task::AbortHandle);

impl GeocodeTicket for AbortTask {
    /// Drops a pending Nominatim request or throttle wait at once: every await in the run
    /// precedes a photo's writes, never falls between them.
    fn cancel(&self) {
        self.0.abort();
    }
}

impl GeocodeBackend for NetGeocode {
    fn run(
        &self,
        app: AppState,
        from: CatalogIdentity,
        abort: Arc<AtomicBool>,
        progress: UnboundedSender<GeocodeProgress>,
        done: GeocodeDone,
    ) -> Box<dyn GeocodeTicket> {
        let task = chairphoto_core::app::runtime().spawn(async move {
            let result = backend::geocode::geocode_all_to_iptc_with(&app, Some(from), &abort, |p| {
                let _ = progress.unbounded_send(p); // the owner is gone: nobody to tell
            })
            .await;
            let _ = done.send(result);
        });
        Box::new(AbortTask(task.abort_handle()))
    }
}

/// What a points read found: the catalog it read and its GPS points, projected.
pub(crate) type PointsRead = Result<(CatalogIdentity, Vec<ProjectedPoint>), String>;

/// Read the open catalog's GPS points and its identity together (blocking: a worker's job).
pub(crate) fn read_points(app: &AppState) -> PointsRead {
    with_catalog_identified(app, |c| {
        backend::ensure_schema_for(c)?;
        Ok(backend::map_photo_points_for(c)?)
    })
    .map(|(from, points)| (from, project_points(&points))) // projected off the UI thread too
}

impl MapState {
    pub fn new(app: AppState, settings: ModuleSettings, model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let _model = cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
            AppModelEvent::CatalogRead => this.reload(cx),
            AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
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
            settings_from: None,
            consent_write_error: None,
            source: TileSource::default(),
            source_error: None,
            applying: None,
            geocode: GeocodeRun::default(),
            geocode_job: None,
            geocode_seq: 0,
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
    pub fn consent(&self, cx: &App) -> Consent {
        if self.stored.is_none() {
            return Consent::Unknown; // the host is not known yet
        }
        self.host_consent(cx).get(self.source.host())
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
        self.settings_from = None;
        self.source = TileSource::default();
        self.source_error = None;
        self.applying = None;
        self.stop_geocode(); // its photo ids are the old catalog's
        self.geocode = GeocodeRun::default();
        cx.notify();
    }

    /// The module is being disabled: nothing it started may keep running.
    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.stop_geocode();
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
        self.run(cx, |app, _| read_points(app), |s, result, _| s.land_points(result));
    }

    /// A points read ([`read_points`]) lands: the points and the catalog they came from.
    pub(crate) fn land_points(&mut self, result: PointsRead) {
        self.points = match result {
            Ok((from, points)) => {
                self.points_from = Some(from);
                Load::Ready(Arc::new(points))
            }
            Err(e) => Load::Failed(e),
        };
        self.points_revision += 1;
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
        let (url_key, hosts_key) = (self.settings.key(TILE_URL_KEY), self.settings.key(TILE_HOSTS_KEY));
        self.run(
            cx,
            move |app, _| {
                with_catalog_identified(app, |c| {
                    // A pure read (`read_catalog_uuid`, never `catalog_uuid`'s `INSERT OR
                    // IGNORE`): a write failing here must not fail the whole settings read
                    // (the tile URL, and the legacy-answers migration, would never apply).
                    Ok((c.get_setting(&url_key)?, c.get_setting(&hosts_key)?, c.read_catalog_uuid()?))
                })
            },
            |s, result, cx| match result {
                Ok((from, (tile_url, legacy, catalog_uuid))) => {
                    s.settings_from = Some(from);
                    s.apply_settings(MapSettingsData { tile_url });
                    match catalog_uuid {
                        Some(catalog_uuid) => s.migrate_consent(from, &catalog_uuid, legacy.as_deref(), cx),
                        // Never happens for a catalog opened through `Catalog::open` (the
                        // migration mints one); left unmigrated rather than guessed at.
                        None => eprintln!("map: catalog has no identity yet; its old tile answers are not migrated"),
                    }
                }
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

    /// The first read of a catalog that still holds per-catalog answers (`map.tileHosts`,
    /// from before they moved to this machine's preferences): fold them into the machine's
    /// ([`HostConsent::merge_legacy_except`]: only allowed/denied entries; denied wins), then
    /// empty the catalog's copy — in the catalog it was read from — so it is merged once, and
    /// a later change in Preferences is not undone by the next read.
    ///
    /// The merge is a durable change ([`MachinePrefs::modify_durably`], #214): it is applied
    /// to this machine's answers as they are when its write runs, becomes visible only once
    /// that write is on disk, and never applies on a store that cannot write (memory only, a
    /// failing disk) — or a restart, which finds the store empty again, would replay it
    /// forever, re-allowing a host the user reset in a session just as undurable. Only then
    /// (`Ok`) is the catalog's copy emptied, off the UI thread. On failure the catalog keeps
    /// its answers (gate #119), the affected hosts keep asking, and
    /// [`Self::consent_write_error`] says why. The copy also survives when a switch lands
    /// before the clear, which `with_catalog_as(from)` then refuses.
    ///
    /// **Merged once per catalog, until its copy reappears with content** (#229, #231). The
    /// merge records the catalog's UUID (`settings.catalog_uuid`) among the machine's answers
    /// ([`HostConsent::record_merged`]), in the same value and so in the same durable write as
    /// the merged answers. A later read of a catalog already recorded, whose copy is not yet
    /// confirmed emptied ([`HostConsent::is_cleared`]) — its clear was refused by a switch, or
    /// has not run yet — merges nothing and only retries the clear: merging it again would let
    /// its old Deny win over an Allow the user gave in between. Once an empty read confirms
    /// the clear ([`Self::confirm_clear`], [`HostConsent::confirm_cleared`]), a *later*
    /// non-empty read of the same UUID is treated as new content — a build from before consent moved to machine prefs (4203b66) writing
    /// into the copy again — so its Denies are folded in, never an Allow (the safe direction
    /// for content with no known provenance), and the catalog goes back to pending for the
    /// clear this content now needs. Those Denies only block a host with no answer; they never
    /// override an Allow. The cleared state is per UUID, not per file: a file copy sharing the
    /// UUID can still hold the very content already merged after another copy's empty read
    /// confirmed the clear, and its old Deny must not undo an Allow given since the first
    /// merge (relD M1).
    ///
    /// A host the user answered since this merge started ([`Self::set_consent`], recorded in
    /// [`ConsentAnswers`]) is left out of it, so the user's Allow, Block or Ask again wins —
    /// even an Allow of a host the catalog denied. A host the user sent back to "ask" earlier
    /// is an explicit "ask" entry, which the merge leaves alone too (#198).
    fn migrate_consent(&mut self, from: CatalogIdentity, catalog_uuid: &str, legacy: Option<&str>, cx: &mut Context<Self>) {
        let legacy = HostConsent::parse(legacy);
        if legacy.is_empty() {
            // The copy is empty: confirm the clear, if that was not already known, so a
            // *later* non-empty read for this catalog is recognized as new content (#231).
            self.confirm_clear(catalog_uuid, cx);
            return;
        }
        // Always attempted, even when the merge turns out to be a no-op (every host already
        // settled, asking again, or the catalog already merged): the catalog may still be
        // holding a copy a previous switch-interrupted clear (#198) left behind, and this is
        // what finally clears it.
        let answers = cx.default_global::<ConsentAnswers>().0.clone();
        let since = lock(&answers).seq;
        let catalog_uuid = catalog_uuid.to_string();
        let merge = move |current: Option<&str>| {
            let mut machine = HostConsent::parse(current);
            let log = lock(&answers);
            if machine.merged_from(&catalog_uuid) {
                if !machine.is_cleared(&catalog_uuid) {
                    return current.unwrap_or_default().to_string(); // pending clear retry: unchanged
                }
                machine.merge_legacy_denies_except(&legacy, |host| log.answered_after(host, since));
            } else {
                machine.merge_legacy_except(&legacy, |host| log.answered_after(host, since));
            }
            machine.record_merged(&catalog_uuid); // (re-)pending: this content still needs clearing
            machine.to_json()
        };
        let (app, key) = (self.app.clone(), self.settings.key(TILE_HOSTS_KEY));
        let (tx, rx) = futures::channel::oneshot::channel();
        MachinePrefs::modify_durably(cx, MACHINE_TILE_HOSTS, merge, move |saved| {
            // Off the UI thread: the clear is catalog I/O, and only follows a merge on disk.
            if saved.is_ok() {
                if let Err(e) = with_catalog_as(&app, from, |c| c.set_setting(&key, "{}")) {
                    eprintln!("map: could not empty the catalog's old tile answers: {e}");
                }
            }
            let _ = tx.send(saved);
        });
        cx.spawn(async move |this, cx| {
            let Ok(saved) = rx.await else { return };
            this.update(cx, |s, cx| {
                if let Err(e) = &saved {
                    eprintln!(
                        "map: this machine's preferences could not be written, so the catalog's old tile \
                         answers were not merged ({e}); the affected hosts will ask again"
                    );
                }
                s.consent_write_error = saved.err();
                cx.notify(); // the merged answers are visible now: repaint
            })
            .ok();
        })
        .detach();
    }

    /// [`Self::migrate_consent`] found `catalog_uuid`'s copy already empty: confirm its clear
    /// on this machine, if that was not already known ([`HostConsent::confirm_cleared`]), so
    /// a *later* non-empty read for it is recognized as new content rather than a retry of
    /// this same clear (#231). A cheap pre-check avoids a durable write on every empty read;
    /// the write itself is a no-op when there is nothing to confirm, so a race with another
    /// confirmation (another switch back to this catalog) is harmless.
    fn confirm_clear(&self, catalog_uuid: &str, cx: &mut Context<Self>) {
        if self.host_consent(cx).is_cleared(catalog_uuid) {
            return;
        }
        let catalog_uuid = catalog_uuid.to_string();
        MachinePrefs::modify_durably(
            cx,
            MACHINE_TILE_HOSTS,
            move |current| {
                let mut machine = HostConsent::parse(current);
                if machine.confirm_cleared(&catalog_uuid) {
                    machine.to_json()
                } else {
                    current.unwrap_or_default().to_string()
                }
            },
            |_| (), // best-effort: nothing reads this outcome; a failed write retries on the next empty read
        );
    }

    /// Every host's answer on this machine (Preferences shows and edits them): what
    /// [`MachinePrefs`] holds — this state keeps no copy that could fall behind it.
    pub fn host_consent(&self, cx: &App) -> HostConsent {
        HostConsent::parse(MachinePrefs::read(cx, MACHINE_TILE_HOSTS).as_deref())
    }

    /// The most recent machine-prefs write for tile consent (a migration or a Preferences
    /// edit) that failed, if any (#214) — shown in Preferences' Map tab so the user knows an
    /// answer, or a legacy migration, is not guaranteed to survive a restart.
    pub fn consent_write_error(&self) -> Option<&str> {
        self.consent_write_error.as_deref()
    }

    /// Remember the answer for `host` on this machine (the consent prompt, or Preferences);
    /// `None` is "Ask again", remembered as such. Applied at once, to the answers as they are
    /// now (an immediate change, [`MachinePrefs::modify_then`]) — unlike the legacy merge
    /// (#214), this is the user's own decision, not an invisible background one — and it wins
    /// over any merge in flight ([`ConsentAnswers`]). A write that fails is surfaced
    /// ([`Self::consent_write_error`]).
    pub fn set_consent(&mut self, host: &str, allowed: Option<bool>, cx: &mut Context<Self>) {
        // Recorded before the change, so a merge that reads the answers after it sees the host
        // as the user's (see `migrate_consent`).
        lock(&cx.default_global::<ConsentAnswers>().0.clone()).answer(host);
        let (tx, rx) = futures::channel::oneshot::channel();
        let host = host.to_string();
        MachinePrefs::modify_then(
            cx,
            MACHINE_TILE_HOSTS,
            move |current| {
                let mut consent = HostConsent::parse(current);
                match allowed {
                    Some(a) => consent.set(&host, a),
                    None => consent.forget(&host),
                }
                consent.to_json()
            },
            move |saved| {
                let _ = tx.send(saved);
            },
        );
        cx.spawn(async move |this, cx| {
            let Ok(saved) = rx.await else { return };
            this.update(cx, |s, cx| {
                s.consent_write_error = saved.err();
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Save a tile URL (empty = the default) in the catalog the settings were read from.
    /// Refused with the reason when unusable, or while that catalog's settings are unread.
    pub fn set_tile_url(&mut self, url: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let source = TileSource::parse(url)?;
        let from = self.settings_from.ok_or("The catalog's map settings are still loading; try again.")?;
        let stored = source.template().to_string();
        self.source = source;
        self.source_error = None;
        if let Some(s) = self.stored.as_mut() {
            s.tile_url = Some(stored.clone());
        }
        cx.notify();
        let key = self.settings.key(TILE_URL_KEY);
        self.run(cx, move |app, _| with_catalog_as(app, from, |c| c.set_setting(&key, &stored)), |_, r, _| {
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
                        Some(id) => backend::apply_fence(c, id)
                            .map(|tagged| backend::FencesApplied { tagged, applied: 1, skipped: Vec::new() }),
                        None => backend::apply_all_fences(c),
                    }
                })
            },
            move |s, r, cx| {
                s.applying = None;
                match r {
                    Ok(out) => {
                        s.status(apply_status(name.as_deref(), &out), cx);
                        s.catalog_changed(cx);
                    }
                    Err(e) => s.status(format!("Failed to apply fence: {e}"), cx),
                }
            },
        );
    }

    /// The id of the Geocode all run in flight.
    pub fn geocode_running(&self) -> Option<u64> {
        self.geocode_job.as_ref().map(|j| j.id)
    }

    /// Stop the run in flight, if any, without a word (a switch, an unload).
    fn stop_geocode(&mut self) {
        if let Some(job) = self.geocode_job.take() {
            job.stop();
        }
    }

    /// The user's Cancel: stop the run; what it filled so far stays filled.
    pub fn cancel_geocode(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.geocode_job.take() else { return };
        job.stop();
        let (done, filled) = (self.geocode.done, self.geocode.filled);
        if filled > 0 {
            self.catalog_changed(cx);
        }
        let line = format!("Geocoding cancelled after {done} photos; {filled} had location fields filled.");
        self.geocode = GeocodeRun { status: line.clone(), ..Default::default() };
        self.status(line, cx);
        cx.notify();
    }

    /// "Geocode all with GPS": Nominatim, ≤ 1 request/s, on the core runtime (network, not a
    /// blocking worker). **Owned:** at most one run; a catalog switch, the module's unload
    /// or Cancel stops it ([`GeocodeJob::stop`]), and its progress and result land only while
    /// it is still the current run — a stopped run's stragglers never touch a newer one.
    pub fn geocode_all(&mut self, cx: &mut Context<Self>) {
        if self.geocode_job.is_some() {
            return;
        }
        let Some(from) = self.points_from else {
            self.status("The catalog is still loading; try again.".into(), cx);
            return;
        };
        self.geocode = GeocodeRun { busy: true, status: "Starting…".into(), ..Default::default() };
        cx.notify();
        self.geocode_seq += 1;
        let id = self.geocode_seq;
        let abort = Arc::new(AtomicBool::new(false));
        let (progress, mut progress_rx) = futures::channel::mpsc::unbounded();
        let (done, done_rx) = futures::channel::oneshot::channel();
        let ticket = MapGeocode::get(cx).run(self.app.clone(), from, abort.clone(), progress, done);
        self.geocode_job = Some(GeocodeJob { id, abort, ticket });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(p) = progress_rx.next().await {
                if this.update(cx, |s, cx| s.land_geocode_progress(id, p, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            // A dropped sender: the run was stopped (and is no longer the current one).
            let result = done_rx.await.unwrap_or_else(|_| Err("Geocoding stopped".into()));
            this.update(cx, |s, cx| s.land_geocode(id, result, cx)).ok();
        })
        .detach();
    }

    /// Progress of run `id`: shown only while it is the current run.
    pub(crate) fn land_geocode_progress(&mut self, id: u64, p: GeocodeProgress, cx: &mut Context<Self>) {
        if self.geocode_running() != Some(id) {
            return; // a stopped or superseded run's straggler
        }
        self.geocode.done = p.done;
        self.geocode.total = p.total;
        self.geocode.filled = p.filled;
        self.geocode.status = format!("{} / {} processed, {} filled…", p.done, p.total, p.filled);
        cx.notify();
    }

    /// The end of run `id`: lands only while it is the current run.
    pub(crate) fn land_geocode(
        &mut self,
        id: u64,
        result: Result<GeocodeAllSummary, String>,
        cx: &mut Context<Self>,
    ) {
        if self.geocode_running() != Some(id) {
            return;
        }
        self.geocode_job = None;
        let line = match result {
            Ok(sum) if sum.total == 0 => "No photos with GPS and empty location fields.".to_string(),
            Ok(sum) => {
                if sum.filled > 0 {
                    self.catalog_changed(cx);
                }
                let mut line = format!("Done: {} of {} photos had location fields filled.", sum.filled, sum.total);
                if sum.skipped > 0 {
                    line.push_str(&format!(" {} already set or no result.", sum.skipped));
                }
                line
            }
            Err(e) => {
                if self.geocode.filled > 0 {
                    self.catalog_changed(cx); // what it filled before failing stays filled
                }
                format!("Geocode failed: {e}")
            }
        };
        self.geocode = GeocodeRun { status: line.clone(), ..Default::default() };
        self.status(line, cx);
        cx.notify();
    }
}

/// The status line for an apply: one fence by `name`, or all of them. Apply all says how
/// many fences it applied and names any it skipped because their tag is an auto-tag (#181),
/// which the catalog assigns by rule and no fence can.
fn apply_status(name: Option<&str>, out: &backend::FencesApplied) -> String {
    let n = out.tagged;
    let photos = if n == 1 { "1 photo".to_string() } else { format!("{n} photos") };
    match name {
        Some(name) => format!("Applied \u{201c}{name}\u{201d}: {photos} newly tagged."),
        None if out.skipped.is_empty() => format!("Applied all fences: {photos} newly tagged."),
        None => {
            let total = out.applied + out.skipped.len();
            let fences = if total == 1 { "fence" } else { "fences" };
            let names: Vec<String> = out.skipped.iter().map(|f| format!("\u{201c}{f}\u{201d}")).collect();
            format!(
                "Applied {} of {total} {fences}: {photos} newly tagged. Skipped {} \u{2014} an auto-tag \
                 can't be assigned by a fence.",
                out.applied,
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A Geocode all backend that only records: the test sends progress and the end.
    use super::*;
    use std::sync::Mutex;

    pub struct Run {
        pub from: CatalogIdentity,
        pub abort: Arc<AtomicBool>,
        pub progress: UnboundedSender<GeocodeProgress>,
        pub done: Option<GeocodeDone>,
        pub cancelled: Arc<AtomicBool>,
    }

    impl Run {
        /// Whether the owner stopped this run (flag and ticket both).
        pub fn stopped(&self) -> bool {
            self.abort.load(Ordering::SeqCst) && self.cancelled.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    pub struct FakeGeocode {
        pub runs: Mutex<Vec<Run>>,
    }

    struct Ticket(Arc<AtomicBool>);

    impl GeocodeTicket for Ticket {
        fn cancel(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl GeocodeBackend for FakeGeocode {
        fn run(
            &self,
            _: AppState,
            from: CatalogIdentity,
            abort: Arc<AtomicBool>,
            progress: UnboundedSender<GeocodeProgress>,
            done: GeocodeDone,
        ) -> Box<dyn GeocodeTicket> {
            let cancelled = Arc::new(AtomicBool::new(false));
            self.runs.lock().unwrap().push(Run { from, abort, progress, done: Some(done), cancelled: cancelled.clone() });
            Box::new(Ticket(cancelled))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback "Nominatim" that never answers: it reports each request it receives, then
    /// the client closing that connection.
    struct HangingNominatim {
        endpoint: String,
        events: std::sync::mpsc::Receiver<&'static str>,
    }

    impl HangingNominatim {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let (tx, events) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let (Ok(mut stream), tx) = (stream, tx.clone()) else { return };
                    std::thread::spawn(move || {
                        use std::io::Read;
                        let mut buf = [0u8; 4096];
                        if stream.read(&mut buf).unwrap_or(0) > 0 {
                            let _ = tx.send("request");
                        }
                        while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
                        let _ = tx.send("closed");
                    });
                }
            });
            HangingNominatim { endpoint, events }
        }

        /// The next event, waiting up to 10 s (the throttle may hold a request ~1 s).
        fn next(&self) -> &'static str {
            self.events.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or("nothing within 10 s")
        }
    }

    /// The real backend's ticket stops a run stuck on Nominatim at once — its pending request
    /// is dropped, not merely flagged for after the answer — and the run ends without a
    /// result or a write. Plain threads: the core runtime and a loopback server.
    #[test]
    fn cancelling_a_net_run_drops_its_pending_request() {
        let nominatim = HangingNominatim::start();
        let dir = crate::tests::TempDir::new("map-net-geocode");
        let root = dir.0.join("photos");
        let file = root.join("p0.jpg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, b"photo").unwrap();
        let catalog = chairphoto_core::catalog::Catalog::open(&dir.0.join("c.chairphoto"), &root).unwrap();
        let id = catalog.upsert_photo(&file, None, 0, 1).unwrap().id;
        backend::ensure_schema_for(&catalog).unwrap();
        backend::set_photo_gps(&catalog, &[id], 59.91, 10.75).unwrap();
        catalog.set_setting(backend::geocode::SETTING_ENDPOINT, &nominatim.endpoint).unwrap();
        let app = AppState::default();
        *app.catalog.lock().unwrap() = Some(catalog);
        let from = chairphoto_core::app::catalog_identity(&app).unwrap();

        let (progress, _progress_rx) = futures::channel::mpsc::unbounded();
        let (done, done_rx) = futures::channel::oneshot::channel();
        let ticket = NetGeocode.run(app.clone(), from, Arc::new(AtomicBool::new(false)), progress, done);
        assert_eq!(nominatim.next(), "request");
        ticket.cancel();
        assert_eq!(nominatim.next(), "closed", "the request outlived the cancel");
        assert!(futures::executor::block_on(done_rx).is_err(), "a stopped run has no result");
        let iptc = chairphoto_core::app::with_catalog(&app, |c| c.get_iptc(id)).unwrap();
        assert!(iptc.city.is_empty());
    }
}
