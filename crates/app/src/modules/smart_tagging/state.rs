//! [`SmarttagsState`]: the Smart Tagging module's live state, shared by the inspector's
//! "Similar tags" panel and the settings panel — the CLIP model's status and download, the
//! index job this app follows, the active photo's kNN suggestions with accept/reject, the
//! model path setting, classifier training and deleting the index.
//!
//! **Off the UI thread.** Every catalog read and write, the model check and download, the job
//! start and training run on the storage [`Runner`]. Each result carries the generation it
//! started under; a catalog switch (or unloading the module) bumps it, so a late answer from
//! the old catalog is dropped.
//!
//! **Catalog identity** (map #92). The settings are read with their identity; Index starts
//! bound to it (`JobFamily::begin_as`), training and Delete index run under it. The active
//! photo's suggestions are read bound to the catalog the Library rows came from, and every
//! suggest/accept/reject runs under that identity, failing closed after a switch.
//!
//! **The index job** — the same ownership as React's panel, without its listener plumbing
//! (every core event reaches the module): one run is followed ([`IndexPhase`]), its events
//! filtered by job id; `smarttags:index_done` is the only way a run ends here (the required
//! terminal signal; never the start's answer), and one that lands before the start's answer is
//! held and replayed. A run already going when the module loads, or in the catalog switched to,
//! is re-adopted from the core's status slot — which the worker clears before its terminal
//! event, only while it owns it — and a run whose end was already seen is never re-adopted.
//! Cancel names its job, so it cannot stop a newer run. `smarttags:download_progress` is
//! cosmetic: it moves the Download label only while a download of ours runs.
//!
//! The only network use is "Download model" (the pinned, checksum-verified CLIP model), on
//! the user's click; photos and embeddings never leave the machine.

use super::logic::index_done_message;
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::smarttags::{self as core_st, SmarttagsSuggestion};
use chairphoto_core::app::{
    with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent, SmarttagsIndexDone,
};
use chairphoto_core::catalog::Catalog;
use chairphoto_core::plugins::smarttags::ModelStatus;
use gpui_kit::{App, Context, Entity, Global, SharedString, Subscription};
use std::collections::HashSet;
use std::sync::Arc;

/// The setting key (`smarttags.<key>`) the backend reads (`models::MODEL_PATH_SETTING`).
pub const MODEL_PATH_KEY: &str = "model_path";

/// What the module asks of the backend that a test must not do for real (stat or download
/// the CLIP model, start the ONNX worker). Blocking: called on the [`Runner`].
pub trait SmarttagsBackend: Send + Sync {
    fn model_status(&self, app: &AppState) -> Result<ModelStatus, String>;
    /// Download the pinned model (network: only on the user's click), then its status.
    fn download_model(&self, app: &AppState) -> Result<ModelStatus, String>;
    /// Start an index bound to `from`; its id.
    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String>;
}

/// The real backend: the core's `app::smarttags`.
pub struct CoreSmarttags;

impl SmarttagsBackend for CoreSmarttags {
    fn model_status(&self, app: &AppState) -> Result<ModelStatus, String> {
        core_st::model_status(app)
    }

    fn download_model(&self, app: &AppState) -> Result<ModelStatus, String> {
        chairphoto_core::app::runtime().block_on(core_st::download_model(app))
    }

    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String> {
        core_st::start_index(app, Some(from))
    }
}

/// The installed backend, a GPUI global; absent means [`CoreSmarttags`].
#[derive(Clone)]
pub struct SmarttagsBackendGlobal(pub Arc<dyn SmarttagsBackend>);

impl Global for SmarttagsBackendGlobal {}

impl SmarttagsBackendGlobal {
    pub fn get(cx: &App) -> Arc<dyn SmarttagsBackend> {
        cx.try_global::<SmarttagsBackendGlobal>()
            .map_or_else(|| Arc::new(CoreSmarttags) as Arc<dyn SmarttagsBackend>, |g| g.0.clone())
    }
}

/// Where the followed index run stands.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum IndexPhase {
    #[default]
    Idle,
    /// "Index" was clicked; the start's answer (the job id) is on its way.
    Starting,
    /// Following `job`; `progress` once a progress event (or the re-attach snapshot) gave
    /// numbers.
    Running { job: u64, done: usize, total: usize, progress: bool },
}

#[derive(Clone, Default)]
pub struct IndexRun {
    pub phase: IndexPhase,
    /// How the last run ended (stays until the next start).
    pub last_result: Option<String>,
    pub error: Option<String>,
    /// Events for a job whose id the start has not answered yet.
    early: Vec<CoreEvent>,
}

impl IndexRun {
    pub fn busy(&self) -> bool {
        self.phase != IndexPhase::Idle
    }

    pub fn job(&self) -> Option<u64> {
        match self.phase {
            IndexPhase::Running { job, .. } => Some(job),
            _ => None,
        }
    }
}

/// The active photo's pending suggestions, read bound to `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoSuggestions {
    pub photo_id: i64,
    pub from: CatalogIdentity,
    pub list: Vec<SmarttagsSuggestion>,
}

#[derive(Debug, Clone, Default)]
pub enum PhotoView {
    #[default]
    None,
    Loading(i64),
    Ready(PhotoSuggestions),
    Failed(i64, String),
}

pub struct SmarttagsState {
    app: AppState,
    settings: ModuleSettings,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    pub model_status: Option<ModelStatus>,
    pub downloading: bool,
    /// The download's bytes (cosmetic), while one of ours runs.
    pub download_progress: Option<(u64, Option<u64>)>,
    pub download_error: Option<String>,
    /// `smarttags.model_path` as stored; `None` until read.
    pub model_path: Option<String>,
    settings_from: Option<CatalogIdentity>,
    pub saves: u64,
    pub index: IndexRun,
    /// Index jobs whose `smarttags:index_done` has been seen: never re-adopted.
    finished: HashSet<u64>,
    pub photo: PhotoView,
    photo_key: Option<(i64, CatalogIdentity)>,
    photo_seq: u64,
    /// "Suggest" is running.
    pub suggesting: bool,
    pub error: Option<String>,
    pub training: bool,
    pub train_status: Option<String>,
    pub deleting: bool,
    pub delete_error: Option<String>,
    generation: u64,
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl SmarttagsState {
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
                AppModelEvent::Core(e) => this.core_event(e, cx),
                _ => {}
            }),
            cx.observe(&shell, |this, _, cx| this.follow_photo(false, cx)),
        ];
        let mut this = SmarttagsState {
            app,
            settings,
            model: model.clone(),
            shell,
            model_status: None,
            downloading: false,
            download_progress: None,
            download_error: None,
            model_path: None,
            settings_from: None,
            saves: 0,
            index: IndexRun::default(),
            finished: HashSet::new(),
            photo: PhotoView::None,
            photo_key: None,
            photo_seq: 0,
            suggesting: false,
            error: None,
            training: false,
            train_status: None,
            deleting: false,
            delete_error: None,
            generation: 0,
            live: true,
            _subscriptions: subscriptions,
        };
        this.check_model(cx);
        if model.read(cx).catalog.is_some() {
            this.catalog_read(cx);
        }
        this
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn model_ready(&self) -> bool {
        self.model_status.as_ref().is_some_and(|m| m.ready)
    }

    /// Whether "Index" may start now.
    pub fn can_index(&self) -> bool {
        self.live && !self.index.busy() && self.model_ready() && self.settings_from.is_some()
    }

    fn run<R: Send + 'static>(
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

    fn changed(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    // --- catalog lifecycle ----------------------------------------------------------------

    fn catalog_read(&mut self, cx: &mut Context<Self>) {
        self.reload_settings(cx);
        self.check_model(cx); // the model path is a per-catalog setting
        self.reattach(cx);
        self.follow_photo(true, cx);
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.model_path = None;
        self.settings_from = None;
        // The core tripped the old catalog's run and cleared its slot; its terminal event
        // still arrives under the old id and is ignored (no longer followed).
        self.index = IndexRun::default();
        self.photo = PhotoView::None;
        self.photo_key = None;
        self.suggesting = false;
        self.error = None;
        self.training = false;
        self.train_status = None;
        self.deleting = false;
        self.delete_error = None;
        cx.notify();
    }

    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.generation += 1;
        cx.notify();
    }

    fn core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::CatalogSwitched(_) => self.catalog_switched(cx),
            CoreEvent::SmarttagsProgress(p) => {
                match &mut self.index.phase {
                    IndexPhase::Running { job, done, total, progress } if *job == p.job => {
                        (*done, *total, *progress) = (p.done, p.total, true);
                    }
                    IndexPhase::Starting => self.index.early.push(event.clone()),
                    _ => return, // another run's straggler
                }
                cx.notify();
            }
            CoreEvent::SmarttagsIndexDone(d) => {
                self.finished.insert(d.job);
                match self.index.phase {
                    IndexPhase::Running { job, .. } if job == d.job => self.finish(d, cx),
                    IndexPhase::Starting => self.index.early.push(event.clone()),
                    _ => {}
                }
            }
            CoreEvent::SmarttagsDownloadProgress(p) => {
                if self.downloading {
                    self.download_progress = Some((p.done, p.total));
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    // --- the model and settings -----------------------------------------------------------

    pub fn check_model(&mut self, cx: &mut Context<Self>) {
        let backend = SmarttagsBackendGlobal::get(cx);
        self.run(cx, move |app| backend.model_status(app), |s, status, _| s.model_status = status.ok());
    }

    /// "Download model (~350 MB)": the user's explicit request for this one transfer (the
    /// pinned model; no photo data). A second click while one runs does nothing.
    pub fn download(&mut self, cx: &mut Context<Self>) {
        if self.downloading {
            return;
        }
        self.downloading = true;
        self.download_progress = None;
        self.download_error = None;
        cx.notify();
        let backend = SmarttagsBackendGlobal::get(cx);
        let app = self.app.clone();
        // The model is per machine: a switch does not void the answer.
        let rx = Runner::get(cx).run(move || backend.download_model(&app));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |s, cx| {
                s.downloading = false;
                s.download_progress = None;
                match result {
                    Ok(Ok(status)) => {
                        let line = if status.ready { "Smart Tagging model downloaded." } else { "Model still not ready." };
                        s.status(line, cx);
                        s.model_status = Some(status);
                    }
                    Ok(Err(e)) => s.download_error = Some(e),
                    Err(_) => s.download_error = Some("Download failed".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let key = self.settings.key(MODEL_PATH_KEY);
        self.run(
            cx,
            move |app| with_catalog_identified(app, |c| Ok(c.get_setting(&key)?.unwrap_or_default())),
            |s, result, _| match result {
                Ok((from, path)) => {
                    s.settings_from = Some(from);
                    s.model_path = Some(path);
                }
                Err(e) => eprintln!("smarttags: settings unavailable: {e}"),
            },
        );
    }

    /// "Save Smart Tagging settings": the model path (blank = the pinned default), into the
    /// catalog it was read from; then the model is checked again.
    pub fn save_model_path(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else {
            self.status("Smart Tagging: the settings are still loading; try again.", cx);
            return;
        };
        let path = path.trim().to_string();
        let key = self.settings.key(MODEL_PATH_KEY);
        let stored = path.clone();
        self.run(cx, move |app| with_catalog_as(app, from, |c| c.set_setting(&key, &path)), move |s, result, cx| match result {
            Ok(()) => {
                s.model_path = Some(stored);
                s.saves += 1;
                s.check_model(cx);
            }
            Err(e) => s.status(format!("Smart Tagging: could not save the settings: {e}"), cx),
        });
    }

    /// "Train classifiers", bound to the settings' catalog.
    pub fn train(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else { return };
        if self.training {
            return;
        }
        self.training = true;
        self.train_status = None;
        cx.notify();
        self.run(cx, move |app| core_st::train_classifiers(app, Some(from)), |s, result, _| {
            s.training = false;
            s.train_status = Some(match result {
                Ok(r) => super::logic::train_line(&r),
                Err(e) => e,
            });
        });
    }

    /// "Delete index" (no confirm, as React), bound to the settings' catalog.
    pub fn delete_index(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else { return };
        if self.deleting {
            return;
        }
        self.deleting = true;
        self.delete_error = None;
        cx.notify();
        self.run(cx, move |app| with_catalog_as(app, from, core_st::delete_index), |s, result, cx| {
            s.deleting = false;
            match result {
                Ok(()) => {
                    s.status("Smart Tagging index deleted. Run Index again to rebuild.", cx);
                    s.follow_photo(true, cx);
                }
                Err(e) => s.delete_error = Some(e),
            }
        });
    }

    // --- the index job --------------------------------------------------------------------

    /// "Index".
    pub fn index_photos(&mut self, cx: &mut Context<Self>) {
        if !self.can_index() {
            return;
        }
        let Some(from) = self.settings_from else { return };
        self.index.phase = IndexPhase::Starting;
        self.index.error = None;
        self.index.last_result = None;
        self.index.early.clear();
        cx.notify();
        let backend = SmarttagsBackendGlobal::get(cx);
        self.run(cx, move |app| backend.start_index(app, from), |s, result, cx| {
            if s.index.phase != IndexPhase::Starting {
                return;
            }
            match result {
                Ok(job) => {
                    s.index.phase = IndexPhase::Running { job, done: 0, total: 0, progress: false };
                    for event in std::mem::take(&mut s.index.early) {
                        s.core_event(&event, cx);
                    }
                }
                Err(e) => {
                    s.index.phase = IndexPhase::Idle;
                    s.index.early.clear();
                    s.index.error = Some(format!("Indexing failed: {e}"));
                }
            }
        });
    }

    /// Cancel the followed run — only it (`cancel_index_job`).
    pub fn cancel_index(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.index.job() else { return };
        self.run(cx, move |app| core_st::cancel_index_job(app, job), |s, r, cx| {
            if let Err(e) = r {
                s.status(format!("Smart Tagging: could not cancel: {e}"), cx);
            }
        });
    }

    fn finish(&mut self, d: &SmarttagsIndexDone, cx: &mut Context<Self>) {
        self.index.phase = IndexPhase::Idle;
        self.index.early.clear();
        if d.ok {
            let line = index_done_message(d);
            self.index.last_result = Some(line.clone());
            self.status(line, cx);
            self.changed(cx); // new embeddings may enable suggestions
        } else {
            self.index.error = Some(format!("Indexing failed: {}", d.error.as_deref().unwrap_or("unknown error")));
        }
        cx.notify();
    }

    /// Adopt an index run that is going without us (started before the module loaded, or in
    /// the catalog switched to). Never one whose end was already seen.
    fn reattach(&mut self, cx: &mut Context<Self>) {
        if self.index.busy() || !self.live {
            return;
        }
        self.run(cx, core_st::index_status, |s, result, _| {
            let Ok(slot) = result else { return };
            if s.index.busy() {
                return; // a start of ours came in between
            }
            if let Some(st) = slot.filter(|st| !s.finished.contains(&st.job)) {
                s.index.phase = IndexPhase::Running { job: st.job, done: st.done, total: st.total, progress: true };
            }
        });
    }

    // --- the active photo -----------------------------------------------------------------

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
        self.run(
            cx,
            move |app| with_catalog_as(app, from, |c| core_st::load_suggestions(c, photo_id)),
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

    /// A write keyed by the shown photo's id, bound to the catalog its suggestions were read
    /// from.
    fn photo_write<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Catalog, i64) -> chairphoto_core::catalog::Result<R> + Send + 'static,
        then: impl FnOnce(&mut Self, i64, CatalogIdentity, R, &mut Context<Self>) + 'static,
    ) {
        let Some((photo, from)) = self.suggestions().map(|p| (p.photo_id, p.from)) else {
            self.status("Smart Tagging: the photo's suggestions are still loading; try again.", cx);
            return;
        };
        self.run(cx, move |app| with_catalog_as(app, from, |c| work(c, photo)), move |s, result, cx| match result {
            Ok(v) => then(s, photo, from, v, cx),
            Err(e) => {
                s.suggesting = false;
                s.error = Some(e);
            }
        });
    }

    /// "Suggest": run the kNN engine for the shown photo, then re-read its list.
    pub fn suggest(&mut self, cx: &mut Context<Self>) {
        if self.suggesting || self.suggestions().is_none() {
            return;
        }
        self.suggesting = true;
        self.error = None;
        cx.notify();
        self.photo_write(
            cx,
            |c, photo| {
                core_st::suggest_tags(c, photo)?;
                core_st::load_suggestions(c, photo)
            },
            |s, photo, from, list, _| {
                s.suggesting = false;
                // The user may have moved on while the kNN ran: the list is that photo's, and
                // lands only while it is still the one shown (its rows are stored either way).
                if s.photo_key == Some((photo, from)) {
                    s.photo = PhotoView::Ready(PhotoSuggestions { photo_id: photo, from, list });
                }
            },
        );
    }

    fn drop_path(&mut self, photo: i64, from: CatalogIdentity, path: &str) {
        if let PhotoView::Ready(p) = &mut self.photo {
            if p.photo_id == photo && p.from == from {
                p.list.retain(|s| s.path != path);
            }
        }
    }

    /// "✓ add".
    pub fn accept(&mut self, path: String, cx: &mut Context<Self>) {
        let p = path.clone();
        self.photo_write(cx, move |c, photo| core_st::accept_suggestion(c, photo, &p), move |s, photo, from, _, cx| {
            s.drop_path(photo, from, &path);
            s.status(format!("Tagged: {path}"), cx);
            s.changed(cx);
        });
    }

    /// "✗ reject": not re-proposed for this photo.
    pub fn reject(&mut self, path: String, cx: &mut Context<Self>) {
        let p = path.clone();
        self.photo_write(cx, move |c, photo| core_st::reject_suggestion(c, photo, &p), move |s, photo, from, _, _| {
            s.drop_path(photo, from, &path);
        });
    }
}
