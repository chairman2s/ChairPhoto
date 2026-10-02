//! [`AiState`]: the AI tagging module's live state, shared by the inspector's "AI tags" panel
//! and the settings panel — the stored `ai.*` settings, the Ollama model list, the active
//! photo's suggestions with every accept/reject, the runs (one photo, a region, a follow-up,
//! the grouped burst run) and the bulk cloud confirm.
//!
//! **Off the UI thread.** Catalog reads and writes, the estimate and the provider runs go
//! through the storage [`Runner`]; every result carries the generation it started under, and
//! a catalog switch (or unloading the module) bumps it, so a late answer changes nothing.
//!
//! **Privacy (AGENTS.md).** A photo reaches a provider only through [`AiBackend`], and only
//! from [`AiState::run`] or a confirmed batch, both after [`AiState::may_send`]: Ollama at a
//! loopback URL is local; an Ollama server elsewhere needs that URL allowed, a cloud provider
//! its own saved API key (the per-provider opt-ins), and a cloud batch of
//! more than one photo needs the user's Proceed on the cost confirm, which closes when the
//! engine, the model or the catalog changes. Every run passes the engine the user consented
//! to down to the core ([`Confirmed`]), which refuses when the catalog's settings name another
//! by then — a save landing between the consent and the run sends nothing. The API keys are settings like any other `ai.*`
//! key (as the React app stored them), shown masked, and never logged here.
//!
//! **Catalog identity** (map #92). The settings are read with their identity and written back
//! under it. The active photo's suggestions are read bound to the catalog the Library rows
//! came from (`ShellState::rows_from`), and every accept/reject runs under that identity; a
//! run is bound to it too (the core checks it in each catalog phase), and refuses when the
//! settings it was allowed by were read from another catalog.

use super::logic::{self, api_key_key, default_of, estimate_bulk_cost, is_cloud, model_key};
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::ai::{self as core_ai, AiSuggestion, Confirmed, GroupedDispatchResult, Region};
use chairphoto_core::app::{with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::catalog::Catalog;
use gpui_kit::{App, Context, Entity, Global, SharedString, Subscription};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What the module asks of the provider side — the only path a photo leaves by. Blocking:
/// called on the [`Runner`]. Tests install a fake that counts calls and sends nothing.
///
/// Every run carries the engine the user consented to (`confirmed`: the provider and model
/// shown when they asked, or on the confirm they proceeded with); the core refuses when the
/// catalog's settings name another.
pub trait AiBackend: Send + Sync {
    /// One photo (a region of it, a follow-up), bound to `from`; the photo's pending set.
    fn suggest(
        &self,
        app: &AppState,
        from: CatalogIdentity,
        confirmed: Confirmed,
        photo: i64,
        question: Option<String>,
        region: Option<Region>,
    ) -> Result<Vec<AiSuggestion>, String>;
    /// The grouped burst run over `photos`, bound to `from`.
    fn suggest_grouped(
        &self,
        app: &AppState,
        from: CatalogIdentity,
        confirmed: Confirmed,
        photos: Vec<i64>,
    ) -> Result<GroupedDispatchResult, String>;
    /// The models on the Ollama server at `url` (local; no photo).
    fn ollama_models(&self, url: &str) -> Result<Vec<String>, String>;
}

/// The real backend: the core's `app::ai`, on its runtime.
pub struct CoreAi;

impl AiBackend for CoreAi {
    fn suggest(
        &self,
        app: &AppState,
        from: CatalogIdentity,
        confirmed: Confirmed,
        photo: i64,
        question: Option<String>,
        region: Option<Region>,
    ) -> Result<Vec<AiSuggestion>, String> {
        chairphoto_core::app::runtime().block_on(core_ai::suggest_tags(app, Some(from), Some(confirmed), photo, question, region))
    }

    fn suggest_grouped(
        &self,
        app: &AppState,
        from: CatalogIdentity,
        confirmed: Confirmed,
        photos: Vec<i64>,
    ) -> Result<GroupedDispatchResult, String> {
        chairphoto_core::app::runtime().block_on(core_ai::suggest_tags_grouped(app, Some(from), Some(confirmed), photos))
    }

    fn ollama_models(&self, url: &str) -> Result<Vec<String>, String> {
        chairphoto_core::app::runtime().block_on(core_ai::ollama_models(url))
    }
}

/// The installed backend, a GPUI global; absent means [`CoreAi`].
#[derive(Clone)]
pub struct AiBackendGlobal(pub Arc<dyn AiBackend>);

impl Global for AiBackendGlobal {}

impl AiBackendGlobal {
    pub fn get(cx: &App) -> Arc<dyn AiBackend> {
        cx.try_global::<AiBackendGlobal>().map_or_else(|| Arc::new(CoreAi) as Arc<dyn AiBackend>, |g| g.0.clone())
    }
}

/// The stored `ai.*` settings: every key in [`logic::DEFAULTS`], as stored (blank = unset).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stored(pub BTreeMap<String, String>);

impl Stored {
    /// The stored value, blank included.
    pub fn raw(&self, key: &str) -> &str {
        self.0.get(key).map_or("", String::as_str)
    }

    /// The value in effect: the stored one, or the default when blank (as `read_config`).
    pub fn value(&self, key: &str) -> String {
        let v = self.raw(key);
        if v.is_empty() { default_of(key).to_string() } else { v.to_string() }
    }

    pub fn provider(&self) -> String {
        self.value("provider")
    }

    /// The model the provider runs (the default when none is chosen).
    pub fn model(&self) -> String {
        self.value(model_key(&self.provider()))
    }

    /// The provider's saved API key; empty for Ollama or when none is saved.
    pub fn api_key(&self) -> &str {
        api_key_key(&self.provider()).map_or("", |k| self.raw(k))
    }

    /// Whether these settings opt into sending ([`logic::send_opt_in`]).
    pub fn opt_in(&self) -> Result<(), String> {
        logic::send_opt_in(&self.provider(), self.api_key(), &self.value("ollama_url"), self.raw("ollama_remote_url"))
    }

    /// The engine these settings name — what a run started from them is consenting to.
    pub fn engine(&self) -> Confirmed {
        Confirmed { provider: self.provider(), model: self.model() }
    }
}

/// The bulk cloud confirm (`bulkConfirm`): what the user is asked to send, and what the
/// answer is bound to.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkConfirm {
    /// The clustered set's size.
    pub count: usize,
    /// Photos actually sent — one per burst cluster; the cost is per representative.
    pub representatives: usize,
    pub provider: String,
    pub model: String,
    /// "$0.03", or "unknown".
    pub display: String,
    pub unknown: bool,
    photos: Vec<i64>,
    from: CatalogIdentity,
}

/// The active photo's suggestions, read bound to `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoSuggestions {
    pub photo_id: i64,
    pub from: CatalogIdentity,
    pub list: Vec<AiSuggestion>,
}

#[derive(Debug, Clone, Default)]
pub enum PhotoView {
    #[default]
    None,
    Loading(i64),
    Ready(PhotoSuggestions),
    Failed(i64, String),
}

pub struct AiState {
    app: AppState,
    /// For its key names only (`ai.<key>`): reads and writes carry their own identity.
    settings: ModuleSettings,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    pub stored: Option<Stored>,
    settings_from: Option<CatalogIdentity>,
    /// Bumped by every save, so the settings view can say "Saved".
    pub saves: u64,
    /// The built-in prompt (the Advanced editor's placeholder and "Load default").
    pub default_prompt: String,
    /// The Ollama server last asked and the models it answered (empty: none or unreachable).
    pub ollama: Option<(String, Vec<String>)>,
    ollama_seq: u64,
    pub photo: PhotoView,
    photo_key: Option<(i64, CatalogIdentity)>,
    photo_seq: u64,
    /// A run (one photo or a batch) is in flight.
    pub busy: bool,
    pub error: Option<String>,
    pub batch_msg: Option<String>,
    pub confirm: Option<BulkConfirm>,
    /// The estimate for a confirm is on its way.
    estimating: bool,
    pub region_mode: bool,
    pub region: Option<Region>,
    /// Bumped when a follow-up question was answered: the view clears its input.
    pub asked: u64,
    generation: u64,
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl AiState {
    pub fn new(
        app: AppState,
        settings: ModuleSettings,
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
                AppModelEvent::CatalogRead => this.catalog_read(cx),
                AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
                _ => {}
            }),
            cx.observe(&shell, |this, _, cx| this.follow_photo(false, cx)),
        ];
        let mut this = AiState {
            app,
            settings,
            model: model.clone(),
            shell,
            stored: None,
            settings_from: None,
            saves: 0,
            default_prompt: core_ai::default_prompt().to_string(),
            ollama: None,
            ollama_seq: 0,
            photo: PhotoView::None,
            photo_key: None,
            photo_seq: 0,
            busy: false,
            error: None,
            batch_msg: None,
            confirm: None,
            estimating: false,
            region_mode: false,
            region: None,
            asked: 0,
            generation: 0,
            live: true,
            _subscriptions: subscriptions,
        };
        if model.read(cx).catalog.is_some() {
            this.catalog_read(cx);
        }
        this
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    /// Run `work` off the UI thread and hand its result to `land`, unless the catalog was
    /// switched (or the module unloaded) meanwhile.
    fn run_off<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&AppState) -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let generation = self.generation;
        let app = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&app));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.generation != generation || !s.live {
                    return;
                }
                land(s, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn status(&self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        let line = line.into();
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    /// A tag write landed: catalog-derived views re-read (React's `notifyChange`).
    fn changed(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    // --- catalog lifecycle ----------------------------------------------------------------

    fn catalog_read(&mut self, cx: &mut Context<Self>) {
        self.reload_settings(cx);
        self.follow_photo(true, cx);
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.stored = None;
        self.settings_from = None;
        self.photo = PhotoView::None;
        self.photo_key = None;
        // A run of the old catalog is dropped (its answer is stale); the confirm, bound to the
        // old catalog's photos, closes.
        self.busy = false;
        self.estimating = false;
        self.confirm = None;
        self.batch_msg = None;
        self.error = None;
        self.region = None;
        cx.notify();
    }

    /// Disabled: nothing it started is heard any more.
    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.generation += 1;
        cx.notify();
    }

    // --- settings -------------------------------------------------------------------------

    pub fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<(String, String)> = logic::DEFAULTS.iter().map(|(k, _)| (k.to_string(), self.settings.key(k))).collect();
        self.run_off(
            cx,
            move |app| {
                with_catalog_identified(app, |c| {
                    let mut map = BTreeMap::new();
                    for (k, stored_key) in &keys {
                        map.insert(k.clone(), c.get_setting(stored_key)?.unwrap_or_default());
                    }
                    Ok(Stored(map))
                })
            },
            |s, result, cx| match result {
                Ok((from, stored)) => {
                    s.settings_from = Some(from);
                    let provider = stored.provider();
                    let url = stored.value("ollama_url");
                    s.stored = Some(stored);
                    if !is_cloud(&provider) {
                        s.fetch_ollama(url, cx);
                    }
                }
                // Never the values: they hold the API keys.
                Err(e) => eprintln!("ai: settings unavailable: {e}"),
            },
        );
    }

    /// Write `values` (`ai.<key>` → value) into the catalog the settings were read from, then
    /// take them as the stored ones. Changing the engine or the model closes the confirm.
    pub fn save(&mut self, values: Vec<(&'static str, String)>, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else {
            self.status("AI tagging: the settings are still loading; try again.", cx);
            return;
        };
        let keyed: Vec<(String, String)> = values.iter().map(|(k, v)| (self.settings.key(k), v.clone())).collect();
        self.run_off(
            cx,
            move |app| {
                with_catalog_as(app, from, |c| {
                    for (k, v) in &keyed {
                        c.set_setting(k, v)?;
                    }
                    Ok(())
                })
            },
            move |s, result, cx| match result {
                Ok(()) => {
                    let mut stored = s.stored.clone().unwrap_or_default();
                    for (k, v) in values {
                        stored.0.insert(k.to_string(), v);
                    }
                    s.take_stored(stored, cx);
                    s.saves += 1;
                }
                Err(e) => s.status(format!("AI tagging: could not save the settings: {e}"), cx),
            },
        );
    }

    fn take_stored(&mut self, stored: Stored, cx: &mut Context<Self>) {
        let before = self.stored.as_ref().map(|s| (s.provider(), s.model()));
        let after = (stored.provider(), stored.model());
        if before.as_ref() != Some(&after) {
            // A different engine or model is a different price: the open estimate is void.
            self.confirm = None;
        }
        let ollama_url = stored.value("ollama_url");
        let local = !is_cloud(&after.0);
        self.stored = Some(stored);
        if local && self.ollama.as_ref().map(|(u, _)| u) != Some(&ollama_url) {
            self.fetch_ollama(ollama_url, cx);
        }
        cx.notify();
    }

    /// The inspector's engine picker: stored at once (same key as Preferences).
    pub fn set_provider(&mut self, provider: &str, cx: &mut Context<Self>) {
        if self.confirm.is_some() || !logic::PROVIDERS.iter().any(|(p, _)| *p == provider) {
            return;
        }
        self.save(vec![("provider", provider.to_string())], cx);
    }

    /// The inspector's model picker: the current provider's model key.
    pub fn set_model(&mut self, model: String, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        let Some(provider) = self.stored.as_ref().map(Stored::provider) else { return };
        self.save(vec![(model_key(&provider), model)], cx);
    }

    /// Ask the Ollama server at `url` for its models (local; the model pickers' list).
    pub fn fetch_ollama(&mut self, url: String, cx: &mut Context<Self>) {
        self.ollama_seq += 1;
        let seq = self.ollama_seq;
        let backend = AiBackendGlobal::get(cx);
        let asked = url.clone();
        self.run_off(cx, move |_| backend.ollama_models(&url), move |s, result, _| {
            if s.ollama_seq == seq {
                s.ollama = Some((asked, result.unwrap_or_default()));
            }
        });
    }

    pub fn ollama_models(&self) -> Vec<String> {
        self.ollama.as_ref().map(|(_, m)| m.clone()).unwrap_or_default()
    }

    // --- the active photo -----------------------------------------------------------------

    /// Follow the Library's active photo: read its pending suggestions, bound to the rows'
    /// catalog, when it (or that catalog) changes, or always with `force`.
    pub fn follow_photo(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        let (active, from) = {
            let shell = self.shell.read(cx);
            (shell.library.selection().active_id, shell.rows_from())
        };
        let key = active.zip(from);
        if key == self.photo_key && !force {
            return;
        }
        if self.photo_key.map(|k| k.0) != key.map(|k| k.0) {
            // A box drawn on one photo must not carry to the next (React).
            self.region = None;
            self.error = None;
        }
        let Some((photo_id, from)) = key else {
            self.photo_key = None;
            self.photo = PhotoView::None;
            cx.notify();
            return;
        };
        if self.photo_key != key {
            self.photo = PhotoView::Loading(photo_id);
        }
        self.photo_key = key;
        self.photo_seq += 1;
        let seq = self.photo_seq;
        self.run_off(
            cx,
            move |app| with_catalog_as(app, from, |c| core_ai::load_suggestions(c, photo_id)),
            move |s, result, _| {
                if s.photo_seq != seq {
                    return;
                }
                s.photo = match result {
                    Ok(list) => PhotoView::Ready(PhotoSuggestions { photo_id, from, list }),
                    Err(e) => PhotoView::Failed(photo_id, e),
                };
            },
        );
    }

    pub fn suggestions(&self) -> Option<&PhotoSuggestions> {
        match &self.photo {
            PhotoView::Ready(p) => Some(p),
            _ => None,
        }
    }

    /// The selection the batch and "✓ all (N)" act on.
    pub fn selected(&self, cx: &App) -> Vec<i64> {
        self.shell.read(cx).library.selection().ids.to_vec()
    }

    // --- consent ----------------------------------------------------------------------------

    /// Whether a run may send photos now, and bound to which catalog: the settings must be
    /// read from the catalog the photos are from, and a remote engine needs its opt-in — a
    /// cloud engine its saved API key, an Ollama server not on this machine its allowed URL.
    /// Nothing is read or sent when this refuses.
    pub fn may_send(&self, from: CatalogIdentity) -> Result<(), String> {
        let stored = self.stored.as_ref().filter(|_| self.settings_from == Some(from)).ok_or_else(|| {
            "AI tagging: the settings are still loading; try again.".to_string()
        })?;
        stored.opt_in()
    }

    // --- runs -------------------------------------------------------------------------------

    /// "Suggest tags" / "Suggest for region" / a follow-up question (`question`) / "Re-run
    /// directly" (`with_region` false): one photo, the active one.
    pub fn run(&mut self, question: Option<String>, with_region: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let question = question.map(|q| q.trim().to_string());
        if question.as_deref() == Some("") {
            return;
        }
        let Some((photo, from)) = self.suggestions().map(|p| (p.photo_id, p.from)) else {
            self.status("AI tagging: the photo's suggestions are still loading; try again.", cx);
            return;
        };
        if let Err(e) = self.may_send(from) {
            self.error = Some(e);
            cx.notify();
            return;
        }
        // The engine the panel shows now is the one the user is asking; the core refuses if
        // the catalog names another by the time the run reads it.
        let Some(engine) = self.stored.as_ref().map(Stored::engine) else { return };
        let region = if with_region { self.region } else { None };
        let asking = question.is_some();
        self.busy = true;
        self.error = None;
        cx.notify();
        let backend = AiBackendGlobal::get(cx);
        self.run_off(cx, move |app| backend.suggest(app, from, engine, photo, question, region), move |s, result, _| {
            s.busy = false;
            match result {
                Ok(list) => {
                    if asking {
                        s.asked += 1;
                    }
                    if s.suggestions().is_some_and(|p| p.photo_id == photo && p.from == from) {
                        s.photo = PhotoView::Ready(PhotoSuggestions { photo_id: photo, from, list });
                    }
                }
                Err(e) => s.error = Some(e),
            }
        });
    }

    /// "Suggest for N selected": a local engine (or one photo) runs at once; a cloud engine
    /// with more than one photo first asks — the estimate prices it per burst representative
    /// and the run waits for Proceed ([`Self::proceed`]).
    pub fn run_batch(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.estimating || self.confirm.is_some() {
            return;
        }
        let photos = self.selected(cx);
        let Some(from) = self.shell.read(cx).rows_from() else { return };
        if photos.is_empty() {
            return;
        }
        if let Err(e) = self.may_send(from) {
            self.error = Some(e);
            cx.notify();
            return;
        }
        let Some(stored) = self.stored.clone() else { return };
        let (provider, model) = (stored.provider(), stored.model());
        if !is_cloud(&provider) || photos.len() <= 1 {
            self.dispatch(photos, from, stored.engine(), cx);
            return;
        }
        self.estimating = true;
        self.error = None;
        cx.notify();
        let ids = photos.clone();
        self.run_off(cx, move |app| core_ai::grouped_estimate(app, Some(from), ids), move |s, result, cx| {
            s.estimating = false;
            // The engine or model changed while estimating: this price is for another one.
            if s.stored.as_ref().map(|st| (st.provider(), st.model())) != Some((provider.clone(), model.clone())) {
                return;
            }
            let (count, representatives) = match result {
                Ok(est) => (est.total, est.representatives),
                Err(e) if e == chairphoto_core::app::CATALOG_CHANGED => {
                    s.status(format!("AI tagging: {e}"), cx);
                    return;
                }
                // Worst case: one call per photo.
                Err(_) => (photos.len(), photos.len()),
            };
            let display = estimate_bulk_cost(&model, representatives);
            s.confirm = Some(BulkConfirm {
                count,
                representatives,
                provider,
                display: display.clone().unwrap_or_else(|| "unknown".into()),
                unknown: display.is_none(),
                model,
                photos,
                from,
            });
        });
    }

    /// Proceed on the bulk confirm: the user's consent to send these representatives with
    /// this engine and model, to this catalog's photos.
    pub fn proceed(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else { return };
        let now = self.stored.as_ref().map(|s| (s.provider(), s.model()));
        if now != Some((confirm.provider.clone(), confirm.model.clone())) || self.shell.read(cx).rows_from() != Some(confirm.from) {
            self.error = Some("The engine, model or catalog changed — ask again for a new estimate.".into());
            cx.notify();
            return;
        }
        if let Err(e) = self.may_send(confirm.from) {
            self.error = Some(e);
            cx.notify();
            return;
        }
        let engine = Confirmed { provider: confirm.provider, model: confirm.model };
        self.dispatch(confirm.photos, confirm.from, engine, cx);
    }

    pub fn cancel_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    fn dispatch(&mut self, photos: Vec<i64>, from: CatalogIdentity, engine: Confirmed, cx: &mut Context<Self>) {
        self.busy = true;
        self.confirm = None;
        self.error = None;
        self.batch_msg = Some(format!("Grouping {}…", photos.len()));
        cx.notify();
        let backend = AiBackendGlobal::get(cx);
        self.run_off(cx, move |app| backend.suggest_grouped(app, from, engine, photos), |s, result, cx| {
            s.busy = false;
            match result {
                Ok(r) => s.batch_msg = Some(logic::batch_done_line(r.total, r.representatives, r.propagated)),
                Err(e) => {
                    s.error = Some(e);
                    s.batch_msg = None;
                }
            }
            s.follow_photo(true, cx);
        });
    }

    // --- region -----------------------------------------------------------------------------

    pub fn toggle_region_mode(&mut self, cx: &mut Context<Self>) {
        self.region_mode = !self.region_mode;
        if !self.region_mode {
            self.region = None; // closing the picker clears the box
        }
        cx.notify();
    }

    pub fn set_region(&mut self, region: Option<Region>, cx: &mut Context<Self>) {
        self.region = region;
        cx.notify();
    }

    // --- accept / reject --------------------------------------------------------------------

    /// A write keyed by the shown photo's id, bound to the catalog its suggestions were read
    /// from; then the paths leave the list.
    fn write(
        &mut self,
        paths: Vec<String>,
        what: &'static str,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Catalog, i64) -> chairphoto_core::catalog::Result<usize> + Send + 'static,
        then: impl FnOnce(&mut Self, usize, &mut Context<Self>) + 'static,
    ) {
        let Some((photo, from)) = self.suggestions().map(|p| (p.photo_id, p.from)) else {
            self.status("AI tagging: the photo's suggestions are still loading; try again.", cx);
            return;
        };
        self.run_off(
            cx,
            move |app| with_catalog_as(app, from, |c| work(c, photo)),
            move |s, result, cx| match result {
                Ok(n) => {
                    if let PhotoView::Ready(p) = &mut s.photo {
                        if p.photo_id == photo && p.from == from {
                            p.list.retain(|x| !paths.contains(&x.path));
                        }
                    }
                    then(s, n, cx);
                }
                Err(e) => s.error = Some(format!("Could not {what}: {e}")),
            },
        );
    }

    /// "✓ add".
    pub fn accept(&mut self, path: String, cx: &mut Context<Self>) {
        let p = path.clone();
        self.write(
            vec![path.clone()],
            "add the tag",
            cx,
            move |c, photo| core_ai::accept_suggestion(c, photo, &p).map(|_| 1),
            move |s, _, cx| {
                s.status(format!("Tagged: {path}"), cx);
                s.changed(cx);
            },
        );
    }

    /// "✗ reject": never suggested again for this photo.
    pub fn reject(&mut self, path: String, cx: &mut Context<Self>) {
        let p = path.clone();
        self.write(vec![path], "reject the suggestion", cx, move |c, photo| {
            core_ai::reject_suggestion(c, photo, &p).map(|_| 1)
        }, |_, _, _| {});
    }

    /// "✓ all (N)": the tag on every selected photo (each one's failure skipped, as React).
    /// The selection's ids must be the same catalog's as the shown suggestions.
    pub fn accept_for_all(&mut self, path: String, cx: &mut Context<Self>) {
        let ids = self.selected(cx);
        if self.shell.read(cx).rows_from() != self.suggestions().map(|p| p.from) {
            self.status(format!("AI tagging: {}", chairphoto_core::app::CATALOG_CHANGED), cx);
            return;
        }
        let p = path.clone();
        self.write(
            vec![path.clone()],
            "add the tag",
            cx,
            move |c, _| Ok(ids.iter().filter(|id| core_ai::accept_suggestion(c, **id, &p).is_ok()).count()),
            move |s, n, cx| {
                s.status(format!("Tagged {n} photos: {path}"), cx);
                s.changed(cx);
            },
        );
    }

    /// "✓ accept group (N)": every suggestion propagated from one representative.
    pub fn accept_group(&mut self, paths: Vec<String>, source: Option<String>, cx: &mut Context<Self>) {
        let ps = paths.clone();
        let count = paths.len();
        self.write(
            paths,
            "add the tags",
            cx,
            move |c, photo| Ok(ps.iter().filter(|p| core_ai::accept_suggestion(c, photo, p).is_ok()).count()),
            move |s, _, cx| {
                s.status(format!("Added {count} tags from {}", source.as_deref().unwrap_or("representative")), cx);
                s.changed(cx);
            },
        );
    }

    /// "✗ reject group".
    pub fn reject_group(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        let ps = paths.clone();
        self.write(paths, "reject the suggestions", cx, move |c, photo| {
            Ok(ps.iter().filter(|p| core_ai::reject_suggestion(c, photo, p).is_ok()).count())
        }, |_, _, _| {});
    }
}
