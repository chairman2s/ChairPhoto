//! [`FacesState`]: the Faces module's live state, shared by its three views (the settings
//! panel, the inspector's Faces block and the loupe overlay) — the models and inference
//! lines, the module's settings, the indexing job this app follows, and the shown photo's
//! faces (the inspector's photo, [`FacesState::shown_photo`]) with every per-face write.
//!
//! **Off the UI thread.** Every catalog read and write, the model check and download, and the
//! job start run on the storage [`Runner`] (the core runtime's blocking pool; a manual queue
//! in tests). Each result carries the generation it started under; a catalog switch bumps
//! it, so an answer from before the switch is dropped.
//!
//! **Catalog identity** (map #92). The shown photo's faces are read **bound to the catalog
//! the Library rows came from** (`ShellState::rows_from`) — the photo id is that catalog's —
//! and every write keyed by a face, tag or photo id runs through `with_catalog_as` with that
//! identity, failing closed (`CATALOG_CHANGED`) once another catalog is open, even before
//! `catalog:switched` arrives. The settings are read with their identity and saved under it;
//! "Index faces" starts bound to it (`JobFamily::begin_as`).
//!
//! **The indexing job.** One run at a time is followed ([`IndexPhase`]). Events come through
//! the app model, filtered by job id: a superseded run's stragglers, and the old catalog's run
//! after a switch, change nothing. `faces:index_done` is the only way a run ends here (never
//! the start's return), and an event that lands before the start's answer is held and replayed
//! once the id is known. A run already going when the module loads, or after a switch, is
//! re-attached from the core's status slot; a run whose `faces:index_done` was already seen is
//! never adopted. Cancel names its job (`cancel_index_job`), so it can never stop a newer one.
//! A **matching** run (#130) is not followed here; while one is running "Index faces" stays
//! disabled, as in React, until its `faces:match_done`.

use super::logic::{batch_confirm_message, index_done_message};
use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::faces::{self as core_faces, FaceForPhoto, FacesInferenceInfo, PeopleTags};
use chairphoto_core::app::{
    with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent, FacesIndexDone, FacesJobStatus,
    FacesMatchJobStatus,
};
use chairphoto_core::catalog::{Catalog, Tag};
use chairphoto_core::plugins::faces::models::ModelStatus;
use gpui_kit::{App, Context, Entity, Global, SharedString, Subscription};
use std::collections::HashSet;
use std::sync::Arc;

/// The module's own setting keys (`faces.<key>`), as the matcher reads them.
pub const PEOPLE_ROOT_KEY: &str = "people_root";
pub const THRESHOLD_KEY: &str = "match_threshold";
pub const DEFAULT_THRESHOLD: &str = "0.45";

/// What the module asks of the face backend that a test must not do for real (stat or
/// download the models, start the ONNX worker). Blocking: called on the [`Runner`].
pub trait FacesBackend: Send + Sync {
    fn models_status(&self) -> ModelStatus;
    /// Download the missing models (network: only on the user's click), then their status.
    fn download_models(&self) -> ModelStatus;
    /// Start an index bound to `from`; its id.
    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String>;
}

/// The real backend: the core's models and `app::faces::start_index`.
pub struct CoreFaces;

impl FacesBackend for CoreFaces {
    fn models_status(&self) -> ModelStatus {
        chairphoto_core::plugins::faces::models::status()
    }

    fn download_models(&self) -> ModelStatus {
        chairphoto_core::app::runtime().block_on(chairphoto_core::plugins::faces::models::ensure_all())
    }

    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String> {
        core_faces::start_index(app, Some(from))
    }
}

/// The installed backend, a GPUI global; absent means [`CoreFaces`].
#[derive(Clone)]
pub struct FacesBackendGlobal(pub Arc<dyn FacesBackend>);

impl Global for FacesBackendGlobal {}

impl FacesBackendGlobal {
    pub fn get(cx: &App) -> Arc<dyn FacesBackend> {
        cx.try_global::<FacesBackendGlobal>().map_or_else(|| Arc::new(CoreFaces) as Arc<dyn FacesBackend>, |g| g.0.clone())
    }
}

/// The module's settings as stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredSettings {
    pub people_root: String,
    pub threshold: String,
}

/// Where the followed index run stands.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum IndexPhase {
    #[default]
    Idle,
    /// "Index faces" was clicked; the start's answer (the job id) is on its way.
    Starting,
    /// Following `job`. `progress` once a progress event (or the re-attach snapshot) gave
    /// numbers.
    Running { job: u64, done: usize, total: usize, progress: bool },
}

/// The indexing section's state.
#[derive(Clone, Default)]
pub struct IndexRun {
    pub phase: IndexPhase,
    /// How the last run ended (stays until the next start).
    pub last_result: Option<String>,
    pub error: Option<String>,
    /// A face-matching run is going (started elsewhere; #130 follows it).
    pub match_busy: bool,
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

/// The shown photo's faces, read bound to `from`.
#[derive(Debug, Clone)]
pub struct PhotoFaces {
    pub photo_id: i64,
    pub from: CatalogIdentity,
    pub faces: Vec<FaceForPhoto>,
    pub people: PeopleTags,
}

#[derive(Debug, Clone, Default)]
pub enum PhotoView {
    /// No shown photo, or the rows' catalog is not known yet.
    #[default]
    None,
    Loading(i64),
    Ready(PhotoFaces),
    Failed(i64, String),
}

pub struct FacesState {
    app: AppState,
    /// For its key names only (`faces.<key>`): reads and writes capture their own identity.
    settings: ModuleSettings,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    pub models: Option<ModelStatus>,
    pub models_busy: bool,
    pub models_error: Option<String>,
    pub inference: Option<FacesInferenceInfo>,
    pub speed_note: Option<String>,
    /// `None` until read.
    pub stored: Option<StoredSettings>,
    settings_from: Option<CatalogIdentity>,
    /// Every tag (the people-root type-ahead).
    pub all_tags: Vec<Tag>,
    /// Bumped by every save, so the settings view can say "Saved".
    pub saves: u64,
    pub index: IndexRun,
    /// Index jobs whose `faces:index_done` has been seen: never re-adopted.
    finished: HashSet<u64>,
    pub photo: PhotoView,
    /// What the photo read in flight (or done) is for: `(photo, rows' catalog)`.
    photo_key: Option<(i64, CatalogIdentity)>,
    photo_seq: u64,
    /// The face whose "confirm on N" is in flight.
    pub batch_busy: Option<i64>,
    generation: u64,
    /// Off after unload: nothing new starts.
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl FacesState {
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
        let mut this = FacesState {
            app,
            settings,
            model: model.clone(),
            shell,
            models: None,
            models_busy: false,
            models_error: None,
            inference: None,
            speed_note: None,
            stored: None,
            settings_from: None,
            all_tags: Vec::new(),
            saves: 0,
            index: IndexRun::default(),
            finished: HashSet::new(),
            photo: PhotoView::None,
            photo_key: None,
            photo_seq: 0,
            batch_busy: None,
            generation: 0,
            live: true,
            _subscriptions: subscriptions,
        };
        this.check_models(cx);
        // Enabled after the first catalog read (the usual case): read now.
        if model.read(cx).catalog.is_some() {
            this.catalog_read(cx);
        }
        this
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn models_ready(&self) -> bool {
        self.models.as_ref().is_some_and(|m| m.ready)
    }

    /// Whether "Index faces" may start now.
    pub fn can_index(&self) -> bool {
        self.live && !self.index.busy() && !self.index.match_busy && self.models_ready() && self.settings_from.is_some()
    }

    /// Run `work` off the UI thread and hand its result to `land`, unless the catalog was
    /// switched (or the module unloaded) meanwhile.
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

    /// A write landed: every catalog-derived view re-reads (React's `notifyChange`).
    fn changed(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    // --- catalog lifecycle ----------------------------------------------------------------

    fn catalog_read(&mut self, cx: &mut Context<Self>) {
        self.reload_settings(cx);
        self.reattach(cx);
        self.follow_photo(true, cx);
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.stored = None;
        self.settings_from = None;
        self.all_tags.clear();
        self.inference = None;
        self.speed_note = None;
        // The core tripped the old catalog's run and cleared its slot; its terminal event
        // still arrives under the old id and is ignored. The new catalog is re-attached on
        // its first read.
        self.index = IndexRun::default();
        self.photo = PhotoView::None;
        self.photo_key = None;
        self.batch_busy = None;
        cx.notify();
    }

    /// The module is being disabled: nothing it started is followed any more (the core job,
    /// if any, keeps running detached, as in React).
    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.generation += 1;
        cx.notify();
    }

    fn core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::CatalogSwitched(_) => self.catalog_switched(cx),
            CoreEvent::FacesProgress(p) => {
                match &mut self.index.phase {
                    IndexPhase::Running { job, done, total, progress } if *job == p.job => {
                        (*done, *total, *progress) = (p.done, p.total, true);
                    }
                    IndexPhase::Starting => self.index.early.push(event.clone()),
                    _ => return, // another run's straggler
                }
                cx.notify();
            }
            CoreEvent::FacesIndexDone(d) => {
                self.finished.insert(d.job);
                match self.index.phase {
                    IndexPhase::Running { job, .. } if job == d.job => self.finish(d, cx),
                    IndexPhase::Starting => self.index.early.push(event.clone()),
                    _ => {}
                }
            }
            CoreEvent::FacesMatchProgress(_) => {
                if !self.index.match_busy {
                    self.index.match_busy = true;
                    cx.notify();
                }
            }
            CoreEvent::FacesMatchDone(_) => {
                // A match ended (or a superseded one did): read the slots again rather than
                // assume nothing else is running.
                self.index.match_busy = false;
                self.reattach(cx);
                // New suggestions may show in the inspector and the overlay.
                self.follow_photo(true, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    // --- models and settings --------------------------------------------------------------

    pub fn check_models(&mut self, cx: &mut Context<Self>) {
        let backend = FacesBackendGlobal::get(cx);
        self.run(cx, move |_| backend.models_status(), |s, status, _| s.models = Some(status));
    }

    /// "Download models": the explicit opt-in for this network transfer (models only — no
    /// photo or face data ever leaves the machine).
    pub fn download_models(&mut self, cx: &mut Context<Self>) {
        if self.models_busy {
            return;
        }
        self.models_busy = true;
        self.models_error = None;
        cx.notify();
        let backend = FacesBackendGlobal::get(cx);
        // Models are per machine, not per catalog: a switch does not void the answer.
        let rx = Runner::get(cx).run(move || backend.download_models());
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |s, cx| {
                s.models_busy = false;
                match result {
                    Ok(status) => {
                        if !status.ready {
                            let missing: Vec<String> = status
                                .models
                                .iter()
                                .filter(|m| !m.present)
                                .map(|m| format!("{}{}", m.key, m.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()))
                                .collect();
                            s.models_error = Some(format!("Download failed: {}", missing.join(", ")));
                        }
                        s.models = Some(status);
                    }
                    Err(_) => s.models_error = Some("Download failed".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let (root_key, threshold_key) = (self.settings.key(PEOPLE_ROOT_KEY), self.settings.key(THRESHOLD_KEY));
        self.run(
            cx,
            move |app| {
                with_catalog_identified(app, |c| {
                    let stored = StoredSettings {
                        people_root: c.get_setting(&root_key)?.unwrap_or_default(),
                        threshold: c.get_setting(&threshold_key)?.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| DEFAULT_THRESHOLD.into()),
                    };
                    let info = core_faces::inference_info(c)?;
                    let tags: Vec<Tag> = c.list_tags_with_counts()?.into_iter().map(|t| t.tag).collect();
                    Ok((stored, info, tags))
                })
            },
            |s, result, _| match result {
                Ok((from, (stored, info, tags))) => {
                    s.settings_from = Some(from);
                    s.stored = Some(stored);
                    s.inference = Some(info);
                    s.all_tags = tags;
                }
                Err(e) => eprintln!("faces: settings unavailable: {e}"),
            },
        );
    }

    /// "Save settings": the people root and the threshold (blank = the default), into the
    /// catalog they were read from.
    pub fn save_settings(&mut self, people_root: String, threshold: String, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else {
            self.status("Faces: the settings are still loading; try again.", cx);
            return;
        };
        let threshold = if threshold.trim().is_empty() { DEFAULT_THRESHOLD.to_string() } else { threshold.trim().to_string() };
        let people_root = people_root.trim().to_string();
        let stored = StoredSettings { people_root: people_root.clone(), threshold: threshold.clone() };
        let (root_key, threshold_key) = (self.settings.key(PEOPLE_ROOT_KEY), self.settings.key(THRESHOLD_KEY));
        self.run(
            cx,
            move |app| {
                with_catalog_as(app, from, |c| {
                    c.set_setting(&root_key, &people_root)?;
                    c.set_setting(&threshold_key, &threshold)
                })
            },
            move |s, result, cx| match result {
                Ok(()) => {
                    s.stored = Some(stored);
                    s.saves += 1;
                    s.follow_photo(true, cx); // the picker's people follow the root
                }
                Err(e) => s.status(format!("Faces: could not save the settings: {e}"), cx),
            },
        );
    }

    /// Background / Full. Takes effect from the next indexing run.
    pub fn set_speed(&mut self, speed: &'static str, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else { return };
        self.run(
            cx,
            move |app| with_catalog_as(app, from, |c| core_faces::set_indexing_speed(c, speed)),
            move |s, result, _| match result {
                Ok(()) => {
                    if let Some(info) = &mut s.inference {
                        info.speed = speed.into();
                    }
                    s.speed_note = Some("Saved — applies from the next indexing run".into());
                }
                Err(e) => s.speed_note = Some(e),
            },
        );
    }

    fn reload_inference(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.settings_from else { return };
        self.run(cx, move |app| with_catalog_as(app, from, core_faces::inference_info), |s, r, _| {
            if let Ok(info) = r {
                s.inference = Some(info);
            }
        });
    }

    // --- the indexing job -----------------------------------------------------------------

    /// "Index faces".
    pub fn index_faces(&mut self, cx: &mut Context<Self>) {
        if !self.can_index() {
            return;
        }
        let Some(from) = self.settings_from else { return };
        self.index.phase = IndexPhase::Starting;
        self.index.error = None;
        self.index.last_result = None;
        self.index.early.clear();
        cx.notify();
        let backend = FacesBackendGlobal::get(cx);
        self.run(cx, move |app| backend.start_index(app, from), |s, result, cx| {
            if s.index.phase != IndexPhase::Starting {
                return;
            }
            match result {
                Ok(job) => {
                    s.index.phase = IndexPhase::Running { job, done: 0, total: 0, progress: false };
                    // Replay what arrived before the id was known, in order.
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

    /// Cancel the followed run (only it: `cancel_index_job`).
    pub fn cancel_index(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.index.job() else { return };
        self.index.last_result = Some("Cancelling — stops after the current photo…".into());
        cx.notify();
        self.run(cx, move |app| core_faces::cancel_index_job(app, job), |s, r, cx| {
            if let Err(e) = r {
                s.status(format!("Faces: could not cancel: {e}"), cx);
            }
        });
    }

    fn finish(&mut self, d: &FacesIndexDone, cx: &mut Context<Self>) {
        self.index.phase = IndexPhase::Idle;
        self.index.early.clear();
        if d.ok {
            let line = index_done_message(d);
            self.index.last_result = Some(line.clone());
            self.status(line, cx);
            self.changed(cx); // new faces show in the inspector and the overlay
        } else {
            self.index.last_result = None;
            self.index.error = Some(format!("Indexing failed: {}", d.error.as_deref().unwrap_or("unknown error")));
        }
        self.reload_inference(cx);
        cx.notify();
    }

    /// Adopt an index run that is going without us (started before the module loaded, or
    /// before a panel reopened), and note a running match.
    fn reattach(&mut self, cx: &mut Context<Self>) {
        if self.index.busy() || !self.live {
            return;
        }
        type Slots = (Option<FacesJobStatus>, Option<FacesMatchJobStatus>);
        self.run(
            cx,
            |app| -> Result<Slots, String> { Ok((core_faces::index_status(app)?, app.jobs.faces_match.status()?)) },
            |s, result, _| {
                let Ok((index, matching)) = result else { return };
                s.index.match_busy = matching.is_some();
                if s.index.busy() {
                    return; // a start of ours came in between
                }
                if let Some(st) = index.filter(|st| !s.finished.contains(&st.job)) {
                    s.index.phase = IndexPhase::Running { job: st.job, done: st.done, total: st.total, progress: true };
                }
            },
        );
    }

    // --- the shown photo ------------------------------------------------------------------

    /// The photo whose faces are shown: the one the inspector and the loupes show
    /// ([`ShellState::loupe_target`] — Compare's focused pane while Compare is open, else the
    /// active photo), so the Faces block never acts on a different photo than the inspector
    /// around it.
    pub fn shown_photo(&self, cx: &App) -> Option<i64> {
        self.shell.read(cx).loupe_target().map(|p| p.id)
    }

    /// Follow the shown photo ([`Self::shown_photo`]): read its faces, bound to the rows'
    /// catalog, when it (or that catalog) changes, or always with `force` (after a write).
    pub fn follow_photo(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        let (active, from) = (self.shown_photo(cx), self.shell.read(cx).rows_from());
        let key = active.zip(from);
        if key == self.photo_key && !force {
            return;
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
            move |app| with_catalog_as(app, from, |c| Ok((core_faces::faces_for_photo(c, photo_id)?, core_faces::people_tags(c)?))),
            move |s, result, _| {
                if s.photo_seq != seq {
                    return; // a newer read is on its way
                }
                s.photo = match result {
                    Ok((faces, people)) => PhotoView::Ready(PhotoFaces { photo_id, from, faces, people }),
                    Err(e) => PhotoView::Failed(photo_id, e),
                };
            },
        );
    }

    pub fn faces(&self) -> Option<&PhotoFaces> {
        match &self.photo {
            PhotoView::Ready(p) => Some(p),
            _ => None,
        }
    }

    /// A write keyed by ids from the shown photo's read, bound to that read's catalog; then
    /// the photo re-reads and the app's catalog-derived views refresh.
    fn face_write<R: Send + 'static>(
        &mut self,
        what: &'static str,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Catalog) -> chairphoto_core::catalog::Result<R> + Send + 'static,
        then: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let Some(from) = self.faces().map(|p| p.from) else {
            self.status("Faces: the photo's faces are still loading; try again.", cx);
            return;
        };
        self.run(cx, move |app| with_catalog_as(app, from, work), move |s, result, cx| {
            match result {
                Ok(v) => then(s, v, cx),
                Err(e) => s.status(format!("Faces: could not {what}: {e}"), cx),
            }
            s.follow_photo(true, cx);
            s.changed(cx);
        });
    }

    pub fn accept(&mut self, face: i64, cx: &mut Context<Self>) {
        self.face_write("confirm the face", cx, move |c| core_faces::accept(c, face), |_, _, _| {});
    }

    pub fn reject(&mut self, face: i64, cx: &mut Context<Self>) {
        self.face_write("reject the face", cx, move |c| core_faces::reject(c, face), |_, _, _| {});
    }

    pub fn ignore(&mut self, face: i64, cx: &mut Context<Self>) {
        self.face_write("ignore the face", cx, move |c| core_faces::ignore(c, face), |_, _, _| {});
    }

    pub fn assign(&mut self, face: i64, tag: i64, cx: &mut Context<Self>) {
        self.face_write("assign the face", cx, move |c| core_faces::assign(c, face, tag), |_, _, _| {});
    }

    /// "＋ Create “name”": a new person under the people root, assigned to the face.
    pub fn create_person(&mut self, face: i64, name: String, cx: &mut Context<Self>) {
        let root = self.faces().map(|p| p.people.root.clone()).unwrap_or_default();
        let path = core_faces::person_path(&root, &name);
        self.face_write("create the person", cx, move |c| core_faces::assign_new_person(c, face, &path), |_, _, _| {});
    }

    pub fn delete_drawn(&mut self, face: i64, cx: &mut Context<Self>) {
        self.face_write("delete the box", cx, move |c| core_faces::delete_drawn(c, face), |_, _, _| {});
    }

    /// A drawn box for a face the detector missed, on the shown photo; `then(new face id)`.
    pub fn add_manual(
        &mut self,
        photo: i64,
        bbox: (f64, f64, f64, f64),
        then: impl FnOnce(i64, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.faces().map(|p| p.photo_id) != Some(photo) {
            return;
        }
        let (x, y, w, h) = bbox;
        self.face_write("draw the box", cx, move |c| core_faces::add_manual(c, photo, x, y, w, h), move |_, id, cx| then(id, cx));
    }

    /// The photos "✓✓ confirm on N" acts on: the selection, plus the shown photo (whose faces
    /// the panel shows, [`Self::shown_photo`]).
    pub fn selection_targets(&self, cx: &App) -> Vec<i64> {
        let mut ids = self.shell.read(cx).library.selection().ids.to_vec();
        if let Some(a) = self.shown_photo(cx) {
            if !ids.contains(&a) {
                ids.push(a);
            }
        }
        ids
    }

    /// "✓✓ confirm on N": confirm the face's suggested person on the whole selection, where
    /// the matcher suggested them (#68). The selection's ids and the face's tag must be the
    /// same catalog's: refused when the rows and the faces were read from different ones.
    pub fn accept_on_selection(&mut self, face: &FaceForPhoto, cx: &mut Context<Self>) {
        let Some(tag) = face.person_tag_id else { return };
        if self.batch_busy.is_some() {
            return;
        }
        let ids = self.selection_targets(cx);
        if ids.len() < 2 {
            return;
        }
        if self.shell.read(cx).rows_from() != self.faces().map(|p| p.from) {
            self.status(format!("Faces: {}", chairphoto_core::app::CATALOG_CHANGED), cx);
            return;
        }
        let person = face.person_name.clone().unwrap_or_else(|| "this person".into());
        let Some(from) = self.faces().map(|p| p.from) else { return };
        let count = ids.len();
        self.batch_busy = Some(face.id);
        cx.notify();
        self.run(
            cx,
            move |app| with_catalog_as(app, from, |c| core_faces::accept_person(c, &ids, tag)),
            move |s, result, cx| {
                s.batch_busy = None;
                match result {
                    Ok(out) => s.status(batch_confirm_message(&out, count, &person), cx),
                    Err(e) => s.status(format!("Could not confirm {person} on the selected photos: {e}"), cx),
                }
                s.follow_photo(true, cx);
                s.changed(cx);
            },
        );
    }
}
