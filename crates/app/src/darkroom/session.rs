//! [`Darkroom`]: the Develop surface's state — React's `DevelopSurface` (it outlives each
//! photo) and the state half of `DarkroomView` (one [`OpenPhoto`] per photo, so nothing —
//! working record, pending autosave, stage — crosses from one photo to the next).
//!
//! # Opening and leaving
//!
//! The shell decides what is on the stage (`ShellState::surface`); the Darkroom follows it
//! ([`Darkroom::sync`], run whenever the shell changes). On `Surface::Develop` it opens the
//! shell's active photo, bound to the catalog its row was read from (`ShellState::rows_from`);
//! a different active photo (the filmstrip, ← / →) saves the pending change and opens that
//! one; any other surface saves and leaves — the develop session is released and the
//! Library's rows re-read (covers and version counts may have changed).
//!
//! # The record, the stage, the save
//!
//! Every control produces the next record ([`Darkroom::apply`], the record maths in
//! `chairphoto_model::darkroom::controls`). Each change goes to the [`DarkroomStage`] (fast
//! frames while dragging, the full frame after it settles), schedules the tone strip's zone
//! masses after the settle, and — after [`AUTOSAVE_QUIET`] — commits a history step to the
//! active version through core (`app::editing::write_version_then_refresh_monochrome`), on a
//! worker, one commit at a time. A photo with no version gets "Version N" on its first
//! change. While the RAW is being prepared the autosave waits (the engine a save is stamped
//! with is not decided yet); leaving saves whatever is pending regardless.
//!
//! # Catalog identity
//!
//! Every read and write keyed by the photo or its version is bound to the [`CatalogIdentity`]
//! the photo was opened under (`with_catalog_as` in core). A switch the UI has not heard of
//! yet makes them fail closed — the autosave reports "The catalog changed…" and keeps the
//! change unsaved; `catalog:switched` closes the Darkroom without saving (the photo it shows
//! belongs to a catalog that is no longer open) and returns to the Library. Frames carry the
//! catalog epoch (`EditJob::catalog_epoch`), so a render for the old catalog's photo id can
//! never be adopted by a stage of the new one.
//!
//! # Seams for #112 (the rails)
//!
//! History (Ctrl+Z, the panel, `goto_version_step`), presets, the lens rail, crop/rotate,
//! the version shelf, cover, proof sheet and duels are #112's. They plug in here: a labelled
//! [`Darkroom::apply`] is how a preset, proof or reset becomes one named step; `history`,
//! `version_id` and [`Darkroom::flush`] are what undo, the shelf and "+ New version" need.

use super::stage::{DarkroomStage, FrameTier, SETTLE};
use crate::image_store::{ImageStore, Submit};
use crate::model::{AppModel, AppModelEvent};
use crate::shell::state::{ShellState, Surface};
use crate::storage::Runner;
use chairphoto_core::app::{editing, with_catalog_as, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::catalog::{Photo, PhotoVersion, VersionHistory};
use chairphoto_core::develop_source::DevelopSource;
use chairphoto_core::image_pool::ImageKind;
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::develop_source::{is_preparing, reduce_source, SourceState};
use chairphoto_model::darkroom::filmstrip::{window_around, STRIP_RADIUS};
use chairphoto_model::darkroom::history::{describe_change, should_amend, LastStep};
use chairphoto_model::darkroom::kelvin::{KelvinContext, WbPrefer, WB_SLIDER_KEY};
use chairphoto_model::darkroom::render_timing::{RENDER_TIMING_KEY, RENDER_TIMING_SUMMARY_KEY};
use chairphoto_model::darkroom::stage_json::stage_json_for;
use chairphoto_model::editing::{as_linear_record, is_engine1_version, parse_edit, VersionEdit};
use gpui_kit::{AppContext as _, Context, Entity, Subscription, Task};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Quiet time after a change before it is saved as a history step (React: 600 ms).
pub const AUTOSAVE_QUIET: Duration = Duration::from_millis(600);

/// Where the `.cube` LUTs live: `app::luts_dir` in the app; a test's own folder in tests.
pub type LutsDir = Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>;

/// One photo in the Darkroom.
pub struct OpenPhoto {
    /// Bumped per open: a worker's answer for an earlier open is dropped.
    pub seq: u64,
    pub photo: Photo,
    /// The catalog the photo's row was read from: every read and write here is bound to it.
    pub from: CatalogIdentity,
    /// `AppModel::catalog_epoch` at the open: the stage's frames are this catalog's.
    pub epoch: u64,
    /// The version edited (`None`: the Original — the first change creates one).
    pub version_id: Option<i64>,
    /// A commit created `version_id` but its first write failed: the shell has not been
    /// given the version yet, so the next commit reads its row back.
    version_unlisted: bool,
    /// How many versions the photo had, for the "Version N" a first change creates.
    pub versions_len: usize,
    /// The working record: what the controls show.
    pub working: VersionEdit,
    /// What the version holds (last saved or loaded), as JSON and as a record.
    committed_json: String,
    committed: VersionEdit,
    last_step: Option<LastStep>,
    /// A caller-named change ("Reset", later "Proof: Portra") for the next commit.
    next_label: Option<String>,
    /// The version is resolved: autosave may run.
    pub loaded: bool,
    pub history: Option<VersionHistory>,
    pub source: SourceState,
    /// The version was made on engine 1 (the camera preview): it keeps rendering there.
    pub engine1_version: bool,
    pub stage: Entity<DarkroomStage>,
    /// The sensor-clipping layer, while it is on.
    pub clip_stage: Option<Entity<DarkroomStage>>,
    pub show_clipping: bool,
    /// The tone strip's zone masses for the settled record (empty until measured).
    pub masses: Vec<f32>,
    masses_seq: u64,
    masses_timer: Option<Task<()>>,
    autosave_timer: Option<Task<()>>,
    /// A commit is on the worker; another change waits for it (`commit_again`).
    committing: bool,
    commit_again: bool,
    pub saving: bool,
    _stage_observer: Subscription,
}

impl OpenPhoto {
    /// The working record as saved: stamped with the engine that renders it (engine 1 =
    /// as is; engine 2 = `as_linear_record` with this frame's camera match).
    pub fn stamped(&self, record: &VersionEdit) -> VersionEdit {
        if self.engine() == 2 {
            as_linear_record(record, self.source.camera_ev)
        } else {
            record.clone()
        }
    }

    /// The engine renders and saves use: 1 for an engine-1 version, else the source's.
    pub fn engine(&self) -> u32 {
        if self.engine1_version {
            1
        } else {
            self.source.engine
        }
    }

    /// The token renders name: none for an engine-1 version (it renders from the preview).
    pub fn source_token(&self) -> Option<&str> {
        if self.engine1_version {
            None
        } else {
            self.source.token.as_deref()
        }
    }

    /// Kelvin white balance: on the RAW with an as-shot light.
    pub fn kelvin(&self, prefer: WbPrefer) -> Option<KelvinContext> {
        let as_shot = self.source.as_shot_wb?;
        self.source_token()?;
        Some(KelvinContext { as_shot, prefer })
    }

    pub fn preparing(&self) -> bool {
        is_preparing(self.source.source.as_ref())
    }

    /// Whether the working record differs from what the version holds.
    pub fn dirty(&self) -> bool {
        self.working.to_json() != self.committed_json
    }

    fn stage_json(&self) -> String {
        stage_json_for(&self.stamped(&self.working), false)
    }

    fn clip_json(&self) -> String {
        let geometry = VersionEdit {
            crop: self.working.crop.clone(),
            straighten: self.working.straighten.clone(),
            perspective: self.working.perspective.clone(),
            ..VersionEdit::default()
        };
        stage_json_for(&self.stamped(&geometry), false)
    }
}

/// What a commit's worker answers: the version id it created, if it did — known even when
/// the write after it failed, so "Version N" is created at most once — and the write's
/// result.
struct Commit {
    created: Option<i64>,
    result: Result<Committed, String>,
}

/// A commit's write: the version's row (read back when it was just created or is not yet
/// the shell's), the version written, the history after the step, and the saved JSON.
struct Committed {
    version: Option<PhotoVersion>,
    version_id: i64,
    history: VersionHistory,
    saved: String,
}

/// The Develop surface. See the module docs.
pub struct Darkroom {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    pool: Arc<dyn Submit>,
    luts_dir: LutsDir,
    pub open: Option<OpenPhoto>,
    /// Photos left (a step, ← Library) while a commit of theirs was on the worker. Each is
    /// kept until that commit answers, so a change made meanwhile is committed after it —
    /// to the version it created or wrote, as React's `chainRef` did — and a failure is
    /// reported (the banner and the status line) instead of dropped with the view.
    leaving: Vec<OpenPhoto>,
    seq: u64,
    /// The `.cube` files in the LUT folder (read when the Darkroom opens).
    pub luts: Vec<String>,
    /// The error banner (an autosave or a LUT import that failed).
    pub error: Option<String>,
    /// `develop.wbSlider`: which white balance a fresh RAW edit shows.
    pub wb_prefer: WbPrefer,
    /// `editor.renderTiming`: log every frame and keep the summary.
    pub timing_log: bool,
    /// A develop session is held (opened and not closed yet).
    session_held: bool,
    _subscriptions: [Subscription; 2],
}

impl Darkroom {
    pub fn new(
        model: &Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        pool: Arc<dyn Submit>,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = model.read(cx).state().clone();
        let _subscriptions = [
            cx.observe(&shell, |this, _, cx| this.sync(cx)),
            cx.subscribe(model, |this, _, event: &AppModelEvent, cx| {
                if let AppModelEvent::Core(e) = event {
                    this.on_core_event(e, cx);
                }
            }),
        ];
        Darkroom {
            app,
            model: model.clone(),
            shell,
            images,
            pool,
            luts_dir: Arc::new(chairphoto_core::app::luts_dir),
            open: None,
            leaving: Vec::new(),
            seq: 0,
            luts: Vec::new(),
            error: None,
            wb_prefer: WbPrefer::Kelvin,
            timing_log: false,
            session_held: false,
            _subscriptions,
        }
    }

    /// Tests: render through `pool` (a hand-driven fake) and keep LUTs in `luts_dir`.
    pub fn set_backends(&mut self, pool: Arc<dyn Submit>, luts_dir: LutsDir) {
        self.pool = pool;
        self.luts_dir = luts_dir;
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn images(&self) -> &Entity<ImageStore> {
        &self.images
    }

    /// Kelvin white balance for the open photo, if it has one.
    pub fn kelvin(&self) -> Option<KelvinContext> {
        self.open.as_ref().and_then(|o| o.kelvin(self.wb_prefer))
    }

    // --- following the shell --------------------------------------------------------------

    /// Open, switch or leave as the shell's surface and active photo say.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        let shell = self.shell.read(cx);
        if shell.surface != Surface::Develop {
            if self.open.is_some() || self.session_held {
                self.leave(true, cx);
            }
            return;
        }
        let active = shell.library.selection().active.cloned();
        let from = shell.rows_from();
        let version = shell.active_version().cloned();
        match (active, from) {
            (Some(photo), Some(from)) => {
                if self.open.as_ref().is_some_and(|o| o.photo.id == photo.id && o.from == from) {
                    return;
                }
                self.leave_photo(true, cx);
                self.open_photo(photo, from, version, cx);
            }
            _ => {
                // Nothing to develop (the selection emptied, or the rows are from no catalog
                // yet): back to the Library, as React's Develop needed an active photo.
                self.leave(true, cx);
                self.shell.update(cx, |s, cx| s.show_library(cx));
            }
        }
    }

    fn open_photo(&mut self, photo: Photo, from: CatalogIdentity, version: Option<PhotoVersion>, cx: &mut Context<Self>) {
        self.seq += 1;
        let seq = self.seq;
        let epoch = self.model.read(cx).catalog_epoch;
        let version = version.filter(|v| v.photo_id == photo.id);
        let working = parse_edit(version.as_ref().map(|v| v.edit_json.as_str()));
        let engine1_version = version.is_some() && is_engine1_version(&working);
        let photo_id = photo.id;
        let stage = self.new_stage(photo_id, epoch, SourceToken::Preview, None, cx);
        let _stage_observer = cx.observe(&stage, |_, _, cx| cx.notify());
        let committed_json = working.to_json();
        let open = OpenPhoto {
            seq,
            photo,
            from,
            epoch,
            version_id: version.as_ref().map(|v| v.id),
            version_unlisted: false,
            versions_len: 0,
            committed: working.clone(),
            committed_json,
            working,
            last_step: None,
            next_label: None,
            loaded: false,
            history: None,
            source: SourceState::default(),
            engine1_version,
            stage,
            clip_stage: None,
            show_clipping: false,
            masses: Vec::new(),
            masses_seq: 0,
            masses_timer: None,
            autosave_timer: None,
            committing: false,
            commit_again: false,
            saving: false,
            _stage_observer,
        };
        self.open = Some(open);
        self.error = None;
        self.rendered_changed(cx);
        self.request_strip_thumbs(cx);
        self.resolve_version(seq, version.map(|v| v.id), cx);
        self.open_session(seq, cx);
        self.read_settings(seq, cx);
        cx.notify();
    }

    fn new_stage(
        &self,
        photo_id: i64,
        epoch: u64,
        source: SourceToken,
        clip: Option<String>,
        cx: &mut Context<Self>,
    ) -> Entity<DarkroomStage> {
        let pool = self.pool.clone();
        let log = self.timing_log;
        cx.new(|cx| {
            let json = clip.clone().unwrap_or_default();
            let mut stage = DarkroomStage::new(pool, photo_id, epoch, source, json, cx);
            stage.set_timing_log(log);
            if clip.is_some() {
                stage.clip_layer()
            } else {
                stage
            }
        })
    }

    /// Run `work` on a worker; `done` runs with its answer only while open `seq` is.
    fn run<T: Send + 'static>(
        &self,
        seq: u64,
        work: impl FnOnce(&AppState) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&state));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |this, cx| {
                if this.open.as_ref().is_some_and(|o| o.seq == seq) {
                    done(this, result, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The shell's active version when it is this photo's, else the first, else none until
    /// the first change; and its history.
    fn resolve_version(&mut self, seq: u64, active: Option<i64>, cx: &mut Context<Self>) {
        let from = self.open.as_ref().map(|o| o.from).expect("open");
        let photo_id = self.open.as_ref().map(|o| o.photo.id).expect("open");
        self.run(
            seq,
            move |state| {
                with_catalog_as(state, from, |c| {
                    let versions = c.list_versions(photo_id)?;
                    let v = versions.iter().find(|v| Some(v.id) == active).or(versions.first()).cloned();
                    let history = match &v {
                        Some(v) => Some(c.version_history(v.id)?),
                        None => None,
                    };
                    Ok((versions.len(), v, history))
                })
            },
            |this, result, cx| {
                let Some(open) = this.open.as_mut() else { return };
                match result {
                    Ok((n, v, history)) => {
                        let before = (open.source_token().map(str::to_string), open.stage_json());
                        open.versions_len = n;
                        if let Some(v) = &v {
                            if Some(v.id) != open.version_id {
                                // Not the shell's version: adopt what this one holds.
                                let record = parse_edit(Some(&v.edit_json));
                                open.committed_json = record.to_json();
                                open.committed = record.clone();
                                open.working = record;
                                open.last_step = None;
                                let v = v.clone();
                                this.shell.update(cx, |s, cx| s.set_active_version(Some(v), cx));
                            }
                        }
                        let open = this.open.as_mut().expect("open");
                        open.engine1_version = v.as_ref().is_some_and(|v| is_engine1_version(&parse_edit(Some(&v.edit_json))));
                        open.version_id = v.map(|v| v.id);
                        open.history = history;
                        open.loaded = true;
                        this.restage(before, cx);
                        this.schedule_autosave(cx);
                    }
                    Err(e) => {
                        // Versions unreadable (or the catalog changed): autosave stays off, so
                        // nothing is written over a version this view never resolved.
                        this.error = Some(format!("Could not read the versions: {e}"));
                    }
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Claim the develop session for the photo (its RAW starts preparing) with its
    /// neighbours to preload, N+1 first.
    fn open_session(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (from, photo_id) = (open.from, open.photo.id);
        let ids = self.shell.read(cx).library.photo_ids();
        let neighbours: Vec<i64> = match ids.iter().position(|&id| id == photo_id) {
            Some(i) => [i.checked_add(1), i.checked_sub(1)].into_iter().flatten().filter_map(|j| ids.get(j).copied()).collect(),
            None => Vec::new(),
        };
        self.session_held = true;
        self.run(
            seq,
            move |state| editing::develop_open(state, Some(from), photo_id, &neighbours),
            move |this, result, cx| match result {
                Ok(source) => this.on_source(&source, None, cx),
                Err(e) => eprintln!("darkroom: develop open photo {photo_id}: {e}"),
            },
            cx,
        );
    }

    /// `develop.wbSlider`, `editor.renderTiming`, and the LUT folder.
    fn read_settings(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(from) = self.open.as_ref().map(|o| o.from) else { return };
        let luts_dir = self.luts_dir.clone();
        self.run(
            seq,
            move |state| {
                let (wb, timing) = with_catalog_as(state, from, |c| {
                    Ok((c.get_setting(WB_SLIDER_KEY)?, c.get_setting(RENDER_TIMING_KEY)?))
                })?;
                let luts = luts_dir().and_then(|d| editing::list_luts_in(&d)).unwrap_or_default();
                Ok((wb, timing, luts))
            },
            |this, result, cx| {
                if let Ok((wb, timing, luts)) = result {
                    this.wb_prefer = WbPrefer::from_setting(wb.as_deref());
                    this.timing_log = timing.as_deref() == Some("1");
                    this.luts = luts;
                    if let Some(open) = &this.open {
                        let on = this.timing_log;
                        open.stage.update(cx, |s, _| s.set_timing_log(on));
                    }
                    cx.notify();
                }
            },
            cx,
        );
    }

    fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event {
            // The photo on the stage belongs to a catalog that is no longer open: close
            // without saving (the write would be refused anyway) and go back to the Library.
            // Not redundant with the shell's own move to the Library: following that (`sync`
            // → `leave(true)`) would attempt an autosave the new catalog refuses, and re-read
            // the rows before the model has read the new catalog.
            CoreEvent::CatalogSwitched(_) => {
                if self.open.is_some() || self.session_held {
                    self.leave(false, cx);
                    self.error = None;
                    if self.shell.read(cx).surface == Surface::Develop {
                        self.shell.update(cx, |s, cx| s.show_library(cx));
                    }
                }
            }
            #[cfg(feature = "raw")]
            CoreEvent::DevelopSource(e) => {
                let source = e.source.clone();
                self.on_source(&source, Some(e.photo_id), cx);
            }
            _ => {}
        }
    }

    /// A source state for the open photo (the open's answer, or `develop:source`). A new
    /// RAW token moves the stage to the working image: a new stage, so no frame of the
    /// preview path can land after the switch.
    fn on_source(&mut self, source: &DevelopSource, event_photo: Option<i64>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let before = (open.source_token().map(str::to_string), open.stage_json());
        let was_preparing = open.preparing();
        open.source = reduce_source(std::mem::take(&mut open.source), source, event_photo, open.photo.id);
        self.restage(before, cx);
        if was_preparing && !self.open.as_ref().is_some_and(|o| o.preparing()) {
            // The RAW is decided: a change made while it prepared can be saved now.
            self.schedule_autosave(cx);
        }
        cx.notify();
    }

    /// After the source or the engine may have changed (`before`: the token and stage record
    /// then): a new token moves the stage to the other pixels — a new stage, so no frame of the
    /// old source can land after the switch; a changed record re-renders.
    fn restage(&mut self, before: (Option<String>, String), cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let after = open.source_token().map(str::to_string);
        if before.0 != after {
            let token = after.as_deref().and_then(SourceToken::parse).unwrap_or(SourceToken::Preview);
            let (photo_id, epoch) = (open.photo.id, open.epoch);
            let stage = self.new_stage(photo_id, epoch, token, None, cx);
            let open = self.open.as_mut().expect("open");
            let old = std::mem::replace(&mut open.stage, stage.clone());
            open._stage_observer = cx.observe(&stage, |_, _, cx| cx.notify());
            old.update(cx, |s, cx| s.close(cx));
            let had_clip = open.clip_stage.take().map(|c| c.update(cx, |s, cx| s.close(cx))).is_some();
            open.show_clipping = false;
            if had_clip {
                self.set_clipping(true, cx);
            }
            self.rendered_changed(cx);
        } else if before.1 != open.stage_json() {
            self.rendered_changed(cx);
        }
    }

    // --- changes ----------------------------------------------------------------------------

    /// A control produced the next record. `label` names the change for history when the
    /// caller knows it better than a diff ("Reset"; #112's "Preset: X", "Proof: Y").
    pub fn apply(&mut self, next: VersionEdit, label: Option<&str>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if next == open.working {
            return;
        }
        open.working = next;
        if let Some(l) = label {
            open.next_label = Some(l.to_string());
        }
        self.rendered_changed(cx);
        self.schedule_autosave(cx);
        cx.notify();
    }

    /// "Reset": back to as shot — every adjustment cleared, framing included.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.apply(VersionEdit::default(), Some("Reset"), cx);
    }

    /// The record the stage shows changed: a fast frame now (throttled), the full one after
    /// the settle, then the zone masses for the full record.
    fn rendered_changed(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let json = open.stage_json();
        open.stage.update(cx, |s, cx| s.edit_changed(json, cx));
        open.masses_seq += 1;
        let masses_seq = open.masses_seq;
        let timer = cx.background_executor().timer(SETTLE);
        open.masses_timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| this.measure_masses(masses_seq, cx)).ok();
        }));
    }

    fn measure_masses(&mut self, masses_seq: u64, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (seq, from, photo_id) = (open.seq, open.from, open.photo.id);
        let full = open.stamped(&open.working).to_json();
        let token = open.source_token().map(str::to_string);
        self.run(
            seq,
            move |state| editing::zone_masses(state, Some(from), photo_id, &full, token.as_deref()),
            move |this, result, cx| {
                let Some(open) = this.open.as_mut() else { return };
                // Cosmetic: a failure leaves the last fill.
                if let Ok(m) = result {
                    if open.masses_seq == masses_seq {
                        open.masses = m.to_vec();
                        cx.notify();
                    }
                }
            },
            cx,
        );
    }

    /// The ◩ Clipping toggle: a second stage renders the sensor-clipping overlay for the
    /// geometry (tone does not move it, so slider drags do not refetch it).
    pub fn set_clipping(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let token = open.source_token().and_then(SourceToken::parse);
        let (photo_id, epoch) = (open.photo.id, open.epoch);
        let json = open.clip_json();
        let clip_stage = match (on, token) {
            (true, Some(token)) => {
                let stage = self.new_stage(photo_id, epoch, token, Some(json), cx);
                stage.update(cx, |s, cx| {
                    s.request(FrameTier::Full, cx);
                });
                Some(stage)
            }
            _ => None,
        };
        let open = self.open.as_mut().expect("open");
        if let Some(old) = std::mem::replace(&mut open.clip_stage, clip_stage) {
            old.update(cx, |s, cx| s.close(cx));
        }
        open.show_clipping = on && open.clip_stage.is_some();
        cx.notify();
    }

    // --- autosave -----------------------------------------------------------------------------

    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if !open.loaded || open.preparing() || !open.dirty() {
            open.autosave_timer = None;
            return;
        }
        let seq = open.seq;
        let timer = cx.background_executor().timer(AUTOSAVE_QUIET);
        open.autosave_timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                if this.open.as_ref().is_some_and(|o| o.seq == seq) {
                    this.flush(cx);
                }
            })
            .ok();
        }));
    }

    /// Save the working record now, if it differs from what the version holds (Ctrl+S, a
    /// step, a switch, leaving). One commit at a time: a flush while one runs follows it.
    pub fn flush(&mut self, cx: &mut Context<Self>) {
        if let Some(seq) = self.open.as_ref().map(|o| o.seq) {
            self.commit(seq, cx);
        }
    }

    /// The open photo, or one being left whose commit is still on the worker, by its `seq`.
    fn photo_mut(&mut self, seq: u64) -> Option<&mut OpenPhoto> {
        match self.open.as_mut() {
            Some(o) if o.seq == seq => Some(o),
            _ => self.leaving.iter_mut().find(|o| o.seq == seq),
        }
    }

    /// [`flush`](Self::flush) for photo `seq`: the open one, or one being left.
    fn commit(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(open) = self.photo_mut(seq) else { return };
        open.autosave_timer = None;
        if !open.loaded || !open.dirty() {
            return;
        }
        if open.committing {
            open.commit_again = true;
            return;
        }
        let record = open.working.clone();
        let change = describe_change(&open.committed, &record, open.next_label.take().as_deref());
        let before = (std::mem::replace(&mut open.committed_json, record.to_json()), std::mem::replace(&mut open.committed, record.clone()));
        let now = now_ms();
        let at_tip = open.history.as_ref().is_some_and(|h| h.head.is_some() && h.head == h.steps.last().map(|s| s.seq));
        let amend = open.version_id.is_some() && should_amend(open.last_step.as_ref(), &change, now, at_tip);
        let saved = open.stamped(&record).to_json();
        let (seq, from, photo_id, vid) = (open.seq, open.from, open.photo.id, open.version_id);
        let name = format!("Version {}", open.versions_len + 1);
        let read_row = vid.is_none() || open.version_unlisted;
        let saved_on_engine1_while_preparing = open.preparing() && open.engine() == 1;
        open.committing = true;
        open.saving = true;
        let label = change.label.clone();
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || -> Commit {
            let (version_id, created) = match vid {
                Some(v) => (v, None),
                None => match with_catalog_as(&state, from, |c| c.create_version(photo_id, &name)) {
                    Ok(v) => (v, Some(v)),
                    Err(e) => return Commit { created: None, result: Err(e) },
                },
            };
            let result = (|| {
                let history = editing::write_version_then_refresh_monochrome(&state, Some(from), version_id, |c| {
                    c.commit_version_edit(version_id, &saved, &label, amend)
                })?;
                let version = if read_row {
                    with_catalog_as(&state, from, |c| c.list_versions(photo_id))?.into_iter().find(|v| v.id == version_id)
                } else {
                    None
                };
                Ok(Committed { version, version_id, history, saved })
            })();
            Commit { created, result }
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Commit { created: None, result: Err("the worker stopped".into()) });
            this.update(cx, |this, cx| this.committed(seq, change, before, saved_on_engine1_while_preparing, result, now, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn committed(
        &mut self,
        seq: u64,
        change: chairphoto_model::darkroom::history::Change,
        before: (String, VersionEdit),
        engine1_while_preparing: bool,
        commit: Commit,
        now: i64,
        cx: &mut Context<Self>,
    ) {
        // The write has happened (or failed) whatever is open now; only the open photo's own
        // commit updates the view. A photo being left takes its answer too: a change made
        // while this commit ran is committed after it, and a failure is reported.
        let is_open = self.open.as_ref().is_some_and(|o| o.seq == seq);
        let Some(open) = self.photo_mut(seq) else { return };
        open.committing = false;
        open.saving = false;
        if let Some(created) = commit.created {
            // Recorded whatever the write did: a failed first write is retried into this
            // version (React set `versionIdRef` right after `createVersion`).
            open.version_id = Some(created);
            open.versions_len += 1;
            open.version_unlisted = true;
        }
        let mut failed = false;
        match commit.result {
            Ok(c) => {
                if engine1_while_preparing && is_engine1_version(&parse_edit(Some(&c.saved))) {
                    open.engine1_version = true;
                }
                open.version_id = Some(c.version_id);
                open.history = Some(c.history);
                open.last_step = Some(LastStep { key: change.key, at: now });
                let photo_id = open.photo.id;
                let active = match c.version {
                    Some(v) => {
                        open.version_unlisted = false;
                        // The shell's active version is the open photo's only.
                        is_open.then(|| PhotoVersion { edit_json: c.saved.clone(), ..v })
                    }
                    None => {
                        let active = self.shell.read(cx).active_version().cloned();
                        active.filter(|v| v.id == c.version_id && v.photo_id == photo_id).map(|v| PhotoVersion { edit_json: c.saved.clone(), ..v })
                    }
                };
                if let Some(v) = active {
                    self.shell.update(cx, |s, cx| s.set_active_version(Some(v), cx));
                }
            }
            Err(e) => {
                open.committed_json = before.0;
                open.committed = before.1;
                failed = true;
                if is_open {
                    self.error = Some(format!("Autosave failed: {e}"));
                } else {
                    // The view has moved on: say which photo, on the banner and the status
                    // line (the Library shows no banner).
                    let name = std::path::Path::new(&open.photo.path)
                        .file_name()
                        .map_or_else(|| open.photo.path.clone(), |n| n.to_string_lossy().into_owned());
                    let line = format!("Autosave failed for {name}: {e}");
                    self.error = Some(line.clone());
                    self.model.update(cx, |m, cx| m.set_status(line, cx));
                }
            }
        }
        let again = self.photo_mut(seq).map(|o| std::mem::take(&mut o.commit_again)).unwrap_or(false);
        if again {
            // A change came in while this commit ran (or was pending when the photo was
            // left): it is the user's, so it is tried.
            self.commit(seq, cx);
        } else if is_open && !failed {
            self.schedule_autosave(cx);
        }
        if !is_open {
            self.finish_leaving(seq, cx);
        }
        // A refused or failed save is not retried on a timer (a switched-away catalog would
        // refuse it forever): the next change, Ctrl+S or leaving tries again.
        cx.notify();
    }

    // --- leaving ------------------------------------------------------------------------------

    /// Leave the open photo: save what is pending (unless `save` is false: the catalog it
    /// belongs to is gone), close its stages.
    fn leave_photo(&mut self, save: bool, cx: &mut Context<Self>) {
        if save {
            // Starts a commit, or — one running — marks the change to follow it.
            self.flush(cx);
        }
        let Some(mut open) = self.open.take() else { return };
        if !save {
            open.commit_again = false;
        }
        if self.timing_log {
            let summary = open.stage.read(cx).timing_summary().to_json();
            eprintln!("[edit-timing] summary {summary}");
            let from = open.from;
            Runner::get(cx).spawn({
                let state = self.app.clone();
                move || {
                    let _ = with_catalog_as(&state, from, |c| c.set_setting(RENDER_TIMING_SUMMARY_KEY, &summary));
                }
            });
        }
        open.stage.update(cx, |s, cx| s.close(cx));
        if let Some(clip) = &open.clip_stage {
            clip.update(cx, |s, cx| s.close(cx));
        }
        if open.committing {
            // Its commit's answer, and the change that may follow it, still belong to it.
            open.autosave_timer = None;
            open.masses_timer = None;
            self.leaving.push(open);
        }
        cx.notify();
    }

    /// A left photo's commit answered: once nothing more is on the worker for it, it is
    /// dropped — and, saved into the catalog the Library shows, the rows are re-read (the
    /// re-read at leaving ran before this save landed).
    fn finish_leaving(&mut self, seq: u64, cx: &mut Context<Self>) {
        let Some(i) = self.leaving.iter().position(|o| o.seq == seq && !o.committing) else { return };
        let left = self.leaving.remove(i);
        let shell = self.shell.read(cx);
        if !left.dirty() && shell.surface != Surface::Develop && shell.rows_from() == Some(left.from) {
            self.shell.update(cx, |s, cx| s.refresh_rows(cx));
        }
    }

    /// Leave Develop: the photo (saving unless `save` is false), the develop session, and a
    /// re-read of the Library's rows (a cover's look or a version count may have changed).
    fn leave(&mut self, save: bool, cx: &mut Context<Self>) {
        let had_photo = self.open.is_some();
        self.leave_photo(save, cx);
        if self.session_held {
            self.session_held = false;
            Runner::get(cx).spawn({
                let state = self.app.clone();
                move || {
                    if let Err(e) = editing::develop_close(&state) {
                        eprintln!("darkroom: develop close: {e}");
                    }
                }
            });
        }
        if save && had_photo {
            self.shell.update(cx, |s, cx| s.refresh_rows(cx));
        }
    }

    /// "Import…": validate a `.cube` file, copy it into the LUT folder (the file itself is
    /// only read), and select it for the photo it was imported on.
    ///
    /// Not [`run`](Self::run): the folder is the Darkroom's, not the photo's, so the list is
    /// refreshed whatever is open when the copy lands; only the selection is the photo's,
    /// and it is applied only while that photo is still the open one (a step in between
    /// leaves the next photo's record alone).
    pub fn import_lut(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(seq) = self.open.as_ref().map(|o| o.seq) else { return };
        let luts_dir = self.luts_dir.clone();
        let rx = Runner::get(cx).run(move || -> Result<(String, Vec<String>), String> {
            let dir = luts_dir()?;
            let name = editing::import_lut_into(&dir, &path)?;
            Ok((name, editing::list_luts_in(&dir)?))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |this, cx| match result {
                Ok((name, luts)) => {
                    this.luts = luts;
                    let next = this
                        .open
                        .as_ref()
                        .filter(|o| o.seq == seq)
                        .map(|o| chairphoto_model::darkroom::controls::set_lut(&o.working, Some(&name)));
                    if let Some(next) = next {
                        this.apply(next, None, cx);
                    }
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(e);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // --- the filmstrip --------------------------------------------------------------------

    /// The strip's photos (the Library's order and filter, a window around the open one).
    pub fn strip(&self, cx: &gpui_kit::App) -> (usize, Vec<Photo>, usize) {
        let shell = self.shell.read(cx);
        let photos = shell.library.photos();
        let ids: Vec<i64> = photos.iter().map(|p| p.id).collect();
        let current = self.open.as_ref().map_or(-1, |o| o.photo.id);
        let w = window_around(&ids, current, STRIP_RADIUS);
        let shown = photos[w.start..w.start + w.ids.len()].to_vec();
        (w.start, shown, photos.len())
    }

    fn request_strip_thumbs(&mut self, cx: &mut Context<Self>) {
        let (_, shown, _) = self.strip(cx);
        let wanted: Vec<(i64, ImageKind)> = shown.iter().map(|p| (p.id, ImageKind::Thumb)).collect();
        self.images.update(cx, |store, _| store.request_batch(&wanted));
    }

    /// Move to `photo_id` (a strip click, ← / →): the shell's active photo changes, and
    /// [`sync`](Self::sync) saves this photo and opens that one.
    pub fn step_to(&mut self, photo_id: i64, cx: &mut Context<Self>) {
        if self.open.as_ref().is_some_and(|o| o.photo.id == photo_id) {
            return;
        }
        self.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(photo_id)));
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
